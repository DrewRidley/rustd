//! Sending the packs of the local store to a place other machines read them from, and
//! fetching theirs: `rustd cache push REMOTE` and `rustd cache pull REMOTE`.
//!
//! A remote holds packs under the names they have locally, which are the hashes of their
//! bytes, and a manifest per project that lists the packs a build of it found useful. A
//! machine that starts with nothing fetches the manifest and then the packs, a few large
//! files, and checks each against its name.

use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Context, bail};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

/// How long a signed request is good for: one pack on a slow line.
const SIGNED_FOR: Duration = Duration::from_secs(60 * 60);
const TRANSFERS: usize = 8;

pub(crate) enum Remote {
    /// A directory, such as a mount shared by the machines.
    Dir(PathBuf),
    S3 {
        bucket: Bucket,
        credentials: Credentials,
        /// With a `/` at its end, or empty.
        prefix: String,
    },
}

impl Remote {
    /// `s3://bucket/prefix`, with the endpoint, region and credentials as the AWS tools take
    /// them from the environment, or the path of a directory.
    pub(crate) fn parse(spec: &str) -> anyhow::Result<Remote> {
        let Some(rest) = spec.strip_prefix("s3://") else {
            return Ok(Remote::Dir(PathBuf::from(spec)));
        };
        let (name, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        let prefix = match prefix.trim_end_matches('/') {
            "" => String::new(),
            prefix => format!("{prefix}/"),
        };
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_owned());
        // Path style is what the servers that are not AWS take.
        let (endpoint, style) = match std::env::var("AWS_ENDPOINT_URL") {
            Ok(endpoint) => (endpoint, UrlStyle::Path),
            Err(_) => (format!("https://s3.{region}.amazonaws.com"), UrlStyle::VirtualHost),
        };
        let endpoint = endpoint.parse().context("AWS_ENDPOINT_URL is not a URL")?;
        let bucket = Bucket::new(endpoint, style, name.to_owned(), region)?;
        let credentials = Credentials::from_env()
            .context("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not set")?;
        Ok(Remote::S3 { bucket, credentials, prefix })
    }

    /// The bytes under `name`, or `None` when there is nothing under it.
    pub(crate) fn fetch(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Remote::Dir(dir) => match std::fs::read(dir.join(name)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Remote::S3 { bucket, credentials, prefix } => {
                let key = format!("{prefix}{name}");
                let url = bucket.get_object(Some(credentials), &key).sign(SIGNED_FOR);
                match ureq::get(url.as_str()).call() {
                    Ok(response) => {
                        let mut bytes = Vec::new();
                        response.into_body().into_reader().read_to_end(&mut bytes)?;
                        Ok(Some(bytes))
                    }
                    Err(ureq::Error::StatusCode(404)) => Ok(None),
                    Err(e) => Err(e).with_context(|| format!("fetching {name}")),
                }
            }
        }
    }

    pub(crate) fn has(&self, name: &str) -> anyhow::Result<bool> {
        match self {
            Remote::Dir(dir) => Ok(dir.join(name).exists()),
            Remote::S3 { bucket, credentials, prefix } => {
                let key = format!("{prefix}{name}");
                let url = bucket.head_object(Some(credentials), &key).sign(SIGNED_FOR);
                match ureq::head(url.as_str()).call() {
                    Ok(_) => Ok(true),
                    Err(ureq::Error::StatusCode(404)) => Ok(false),
                    Err(e) => Err(e).with_context(|| format!("looking for {name}")),
                }
            }
        }
    }

    /// Puts `bytes` under `name`, all of them or none.
    pub(crate) fn send(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        match self {
            Remote::Dir(dir) => {
                let to = dir.join(name);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                write_whole(&to, bytes)
            }
            Remote::S3 { bucket, credentials, prefix } => {
                let key = format!("{prefix}{name}");
                let url = bucket.put_object(Some(credentials), &key).sign(SIGNED_FOR);
                ureq::put(url.as_str()).send(bytes).with_context(|| format!("sending {name}"))?;
                Ok(())
            }
        }
    }
}

/// Writes a file that readers see all of or not at all.
fn write_whole(to: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut partial = to.as_os_str().to_owned();
    partial.push(format!(".{}.partial", std::process::id()));
    File::create(&partial)?.write_all(bytes)?;
    std::fs::rename(partial, to)?;
    Ok(())
}

/// What a project's builds were served from: the names of packs, one a line.
struct Manifest(BTreeSet<String>);

impl Manifest {
    fn at(project: &str) -> String {
        format!("manifests/{project}")
    }

    fn fetch(remote: &Remote, project: &str) -> anyhow::Result<Manifest> {
        let bytes = remote.fetch(&Manifest::at(project))?.unwrap_or_default();
        let text = String::from_utf8(bytes).context("the manifest is not text")?;
        Ok(Manifest(text.lines().filter(|it| is_pack(it)).map(str::to_owned).collect()))
    }

    fn send(&self, remote: &Remote, project: &str) -> anyhow::Result<()> {
        let text: String = self.0.iter().flat_map(|it| [it.as_str(), "\n"]).collect();
        remote.send(&Manifest::at(project), text.as_bytes())
    }
}

/// Whether `name` is what a sealed pack is called, and so safe to use as a file name.
fn is_pack(name: &str) -> bool {
    name.strip_suffix(".pack")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|it| it.is_ascii_hexdigit()))
}

fn local_packs(store: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut packs = BTreeSet::new();
    for entry in std::fs::read_dir(store)? {
        if let Ok(name) = entry?.file_name().into_string()
            && is_pack(&name)
        {
            packs.insert(name);
        }
    }
    Ok(packs)
}

/// Runs `work` on each name, a few at a time, and returns how many bytes went by.
pub(crate) fn transfer(
    names: &[&String],
    work: impl Fn(&str) -> anyhow::Result<u64> + Sync,
) -> anyhow::Result<u64> {
    let next = Mutex::new(names.iter());
    let total = Mutex::new(Ok(0));
    std::thread::scope(|scope| {
        for _ in 0..TRANSFERS.min(names.len()) {
            scope.spawn(|| {
                while let Some(name) = next.lock().ok().and_then(|mut it| it.next()) {
                    let done = work(name);
                    let Ok(mut total) = total.lock() else { break };
                    match (&mut *total, done) {
                        (Ok(total), Ok(bytes)) => *total += bytes,
                        (Ok(_), Err(e)) => *total = Err(e),
                        (Err(_), _) => break,
                    }
                }
            });
        }
    });
    total.into_inner().unwrap_or_else(|_| bail!("a transfer panicked"))
}

/// Sends the sealed packs the remote lacks and lists all local ones in the project's manifest.
pub(crate) fn push(store: &Path, remote: &Remote, project: &str) -> anyhow::Result<String> {
    hir_ty::mir::codegen::seal_left_open(store);
    let local = local_packs(store)?;
    let mut manifest = Manifest::fetch(remote, project)?;
    let missing: Vec<&String> = local.iter().filter(|it| !manifest.0.contains(*it)).collect();
    let sent = transfer(&missing, |name| {
        let at = format!("packs/{name}");
        if remote.has(&at)? {
            return Ok(0);
        }
        let bytes = std::fs::read(store.join(name))?;
        remote.send(&at, &bytes)?;
        Ok(bytes.len() as u64)
    })?;
    let listed = missing.len();
    manifest.0.extend(local);
    manifest.send(remote, project)?;
    Ok(format!("{listed} packs added to `{project}`, {} MB sent", sent >> 20))
}

/// Fetches the packs of the project's manifest that are not in the local store.
pub(crate) fn pull(store: &Path, remote: &Remote, project: &str) -> anyhow::Result<String> {
    std::fs::create_dir_all(store)?;
    let local = local_packs(store)?;
    let manifest = Manifest::fetch(remote, project)?;
    let missing: Vec<&String> = manifest.0.difference(&local).collect();
    let fetched = transfer(&missing, |name| {
        let Some(bytes) = remote.fetch(&format!("packs/{name}"))? else {
            bail!("`{project}` lists {name}, which is not there");
        };
        if hir_ty::mir::codegen::pack_name(&bytes).as_deref() != Some(name) {
            bail!("{name} is not what was sealed under that name");
        }
        write_whole(&store.join(name), &bytes)?;
        Ok(bytes.len() as u64)
    })?;
    Ok(format!("{} packs of `{project}` fetched, {} MB", missing.len(), fetched >> 20))
}

/// `rustd cache push|pull REMOTE [PROJECT]`, on the store `RUSTD_CACHE` names.
pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let store = PathBuf::from(std::env::var_os("RUSTD_CACHE").context("RUSTD_CACHE is not set")?);
    let (Some(verb), Some(remote)) = (args.next(), args.next()) else {
        bail!("usage: rustd cache push|pull REMOTE [PROJECT]");
    };
    let project = args.next().unwrap_or_else(|| "default".to_owned());
    if project.contains(['/', '\\']) || project.starts_with('.') {
        bail!("`{project}` is not the name of a project");
    }
    let remote = Remote::parse(&remote)?;
    let start = std::time::Instant::now();
    let done = match verb.as_str() {
        "push" => push(&store, &remote, &project)?,
        "pull" => pull(&store, &remote, &project)?,
        _ => bail!("usage: rustd cache push|pull REMOTE [PROJECT]"),
    };
    println!("{done} in {:.1?}", start.elapsed());
    Ok(())
}

/// The remote the daemon keeps its store in step with: `RUSTD_REMOTE`, and the project
/// `RUSTD_PROJECT` names in it.
pub(crate) struct Shared {
    store: PathBuf,
    remote: Remote,
    project: String,
}

impl Shared {
    /// `None` without a store or a remote. A remote that cannot be reached is not an error
    /// to the build: it compiles what it would have fetched.
    pub(crate) fn from_env() -> Option<Shared> {
        let store = PathBuf::from(std::env::var_os("RUSTD_CACHE")?);
        let remote = std::env::var("RUSTD_REMOTE").ok()?;
        let project = std::env::var("RUSTD_PROJECT").unwrap_or_else(|_| "default".to_owned());
        match Remote::parse(&remote) {
            Ok(remote) => Some(Shared { store, remote, project }),
            Err(e) => {
                eprintln!("remote: {e:#}");
                None
            }
        }
    }

    pub(crate) fn pull(&self) {
        let start = std::time::Instant::now();
        match pull(&self.store, &self.remote, &self.project) {
            Ok(done) => eprintln!("remote: {done} in {:.1?}", start.elapsed()),
            Err(e) => eprintln!("remote: {e:#}"),
        }
    }

    pub(crate) fn push(&self) {
        let start = std::time::Instant::now();
        match push(&self.store, &self.remote, &self.project) {
            Ok(done) => eprintln!("remote: {done} in {:.1?}", start.elapsed()),
            Err(e) => eprintln!("remote: {e:#}"),
        }
    }
}
