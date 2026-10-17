//! Moving one object between the remote and a file, so that the file is either all of what
//! its name says or not there.

use std::{
    fmt,
    fs::TryLockError,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, bail};
use aws_sdk_s3::primitives::ByteStream;
use hir_ty::mir::codegen::name_file_whole;
use sha2::{Digest as _, Sha256};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{Semaphore, SemaphorePermit},
};

/// How often a transfer that broke off is taken up again where it stopped.
const RESUMES: u32 = 6;
/// The wait before the first of those; each one after waits twice as long.
const FIRST_WAIT: Duration = Duration::from_millis(250);
const CHUNK: usize = 1 << 16;

/// The SHA-256 of an object, which is its name on the remote.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Digest([u8; 32]);

impl Digest {
    pub(crate) fn of(bytes: &[u8]) -> Digest {
        Digest(Sha256::digest(bytes).into())
    }

    /// The digest written as 64 hex digits, and nothing else: a name that parses is safe
    /// as the name of a file.
    pub(crate) fn parse(hex: &str) -> Option<Digest> {
        let mut digest = [0; 32];
        if hex.len() != 64 || !hex.is_ascii() {
            return None;
        }
        for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Digest(digest))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// What a small object was when it was read, for a write that must not undo another's.
pub(crate) struct Version(pub(super) Option<String>);

/// The line to the remote that the transfers of a process share: how many requests are on
/// it at a time, and what went over it.
pub(crate) struct Traffic {
    gate: Semaphore,
    jobs: usize,
    /// Set when the remote stopped answering, so that what is still waiting does not each
    /// find that out for itself.
    down: AtomicBool,
    /// Calls of the service, and the attempts they took.
    pub(super) operations: AtomicU64,
    pub(super) requests: AtomicU64,
    /// Transfers taken up again after they broke off.
    pub(super) resumed: AtomicU64,
    pub(super) received: AtomicU64,
    pub(super) sent: AtomicU64,
    /// Objects that were not what their name says.
    pub(super) rejected: AtomicU64,
}

impl Traffic {
    pub(crate) fn new(jobs: usize) -> Traffic {
        let jobs = jobs.max(1);
        Traffic {
            gate: Semaphore::new(jobs),
            jobs,
            down: AtomicBool::new(false),
            operations: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            resumed: AtomicU64::new(0),
            received: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    pub(super) fn jobs(&self) -> usize {
        self.jobs
    }

    /// A place on the line, held for as long as a request and its body take.
    pub(super) async fn permit(&self) -> anyhow::Result<SemaphorePermit<'_>> {
        let permit = self.gate.acquire().await?;
        if self.down.load(Ordering::Relaxed) {
            bail!("the remote stopped answering");
        }
        Ok(permit)
    }

    /// Has the remote tried again after it was taken to be down.
    pub(super) fn try_again(&self) {
        self.down.store(false, Ordering::Relaxed);
    }

    pub(crate) fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
}

impl fmt::Display for Traffic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = |it: &AtomicU64| it.load(Ordering::Relaxed);
        let (operations, requests) = (count(&self.operations), count(&self.requests));
        write!(
            f,
            "{} requests ({} tried again, {} transfers resumed), {} bytes received, {} sent, {} objects rejected",
            requests.max(operations),
            requests.saturating_sub(operations),
            count(&self.resumed),
            count(&self.received),
            count(&self.sent),
            count(&self.rejected),
        )
    }
}

/// The bytes of an object from some offset on, as they arrive.
pub(super) enum Body {
    File(File),
    S3(ByteStream),
}

impl Body {
    async fn next(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Body::File(file) => {
                let mut chunk = vec![0; CHUNK];
                let read = file.read(&mut chunk).await?;
                chunk.truncate(read);
                Ok((read != 0).then_some(chunk))
            }
            Body::S3(stream) => Ok(stream.try_next().await?.map(|it| it.to_vec())),
        }
    }
}

/// A failure that waiting does not cure: the service said no, a list names what is not
/// there, or a file of this machine could not be written.
pub(super) fn is_final(error: &anyhow::Error) -> bool {
    error.is::<Refused>() || error.is::<Missing>() || error.is::<std::io::Error>()
}

impl super::Remote {
    /// Makes a request of the remote until it is answered, with the waits a broken transfer
    /// is taken up after, for as long as waiting may cure what went wrong.
    pub(super) async fn patiently<T, F>(&self, mut request: impl FnMut() -> F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        let mut again = 0;
        loop {
            match request().await {
                Ok(answer) => return Ok(answer),
                Err(e) if is_final(&e) => return Err(e),
                Err(e) if again == RESUMES => {
                    self.traffic.down.store(true, Ordering::Relaxed);
                    return Err(e.context(format!("after asking {again} times more")));
                }
                Err(_) => {
                    tokio::time::sleep(FIRST_WAIT * 2u32.pow(again)).await;
                    again += 1;
                    self.traffic.resumed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// How a fetch ended, when it did not fail.
pub(crate) enum Fetched {
    /// The file is there, and this many bytes came over the line for it.
    Whole(u64),
    /// Another process is fetching the same file.
    Elsewhere,
}

/// How many of the first bytes of an object are kept to tell what it is.
const START: usize = 8;

/// The file an object is fetched into before it has its name: `NAME.fetch.tmp`, which in a
/// store is a temporary file of the store's, locked by the process that writes it. One left
/// by a process that was stopped is taken up where it ends.
struct Partial {
    path: PathBuf,
    file: File,
    hash: Sha256,
    len: u64,
    start: Vec<u8>,
}

impl Partial {
    /// `None` when another process holds it.
    async fn open(to: &Path) -> anyhow::Result<Option<Partial>> {
        let mut path = to.as_os_str().to_owned();
        path.push(".fetch.tmp");
        let path = PathBuf::from(path);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        let file = File::from_std(file);
        let mut partial = Partial { path, file, hash: Sha256::new(), len: 0, start: Vec::new() };
        let mut chunk = vec![0; CHUNK];
        loop {
            let read = partial.file.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            partial.count(&chunk[..read]);
        }
        Ok(Some(partial))
    }

    fn count(&mut self, chunk: &[u8]) {
        let wanted = START.saturating_sub(self.start.len()).min(chunk.len());
        self.start.extend_from_slice(&chunk[..wanted]);
        self.hash.update(chunk);
        self.len += chunk.len() as u64;
    }

    async fn append(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.file.write_all(chunk).await?;
        self.count(chunk);
        Ok(())
    }

    async fn empty(&mut self) -> anyhow::Result<()> {
        self.file.set_len(0).await?;
        self.file.seek(SeekFrom::Start(0)).await?;
        (self.hash, self.len) = (Sha256::new(), 0);
        self.start.clear();
        Ok(())
    }

    fn digest(&self) -> Digest {
        Digest(self.hash.clone().finalize().into())
    }

    /// Gives the file its name, which is when anything else starts to read it. A file that
    /// cannot be given it is removed: the last of its bytes may not have been written.
    async fn seal(mut self, to: &Path) -> anyhow::Result<()> {
        let flushed = self.file.flush().await;
        let (file, from, to) = (self.file.into_std().await, self.path, to.to_owned());
        let named = tokio::task::spawn_blocking(move || {
            let named = flushed.and_then(|()| name_file_whole(&file, &from, &to));
            if named.is_err() {
                _ = std::fs::remove_file(&from);
            }
            named
        });
        Ok(named.await??)
    }

    async fn discard(self) {
        _ = tokio::fs::remove_file(&self.path).await;
    }
}

impl super::Remote {
    /// Fetches the object `name` into the file `to` if its bytes hash to `want` and start as
    /// `is_wanted` says the file has to.
    ///
    /// A transfer that breaks off is taken up from the byte it stopped at, a few times with
    /// a growing wait in between, and by the next process after this one is stopped. Bytes
    /// that are not what was asked for are thrown away and the object is counted as
    /// rejected. A failure that waiting does not cure leaves nothing behind.
    pub(crate) async fn fetch_file(
        &self,
        name: &str,
        want: &Digest,
        to: &Path,
        is_wanted: fn(&[u8]) -> bool,
    ) -> anyhow::Result<Fetched> {
        let _permit = self.traffic.permit().await?;
        let Some(mut partial) = Partial::open(to).await? else {
            return Ok(Fetched::Elsewhere);
        };
        let mut left_by_another = partial.len != 0;
        let mut received = 0;
        let mut resumes = 0;
        loop {
            let had = partial.len;
            let appended = self.append_rest(name, &mut partial, &mut received).await;
            // A transfer that got further is not the same failure again.
            if partial.len > had {
                resumes = 0;
            }
            match appended {
                Ok(()) => {}
                Err(e) if is_final(&e) => {
                    partial.discard().await;
                    return Err(e.context(format!("fetching {name}")));
                }
                Err(e) if resumes == RESUMES => {
                    self.traffic.down.store(true, Ordering::Relaxed);
                    return Err(e.context(format!("fetching {name}, after {resumes} resumes")));
                }
                Err(_) => {
                    tokio::time::sleep(FIRST_WAIT * 2u32.pow(resumes)).await;
                    resumes += 1;
                    self.traffic.resumed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            let got = partial.digest();
            if got == *want && is_wanted(&partial.start) {
                partial.seal(to).await.with_context(|| format!("fetching {name}"))?;
                return Ok(Fetched::Whole(received));
            }
            // What a stopped process left may be the start of something else.
            if std::mem::take(&mut left_by_another) {
                partial.empty().await?;
                continue;
            }
            let len = partial.len;
            partial.discard().await;
            self.traffic.rejected.fetch_add(1, Ordering::Relaxed);
            if got == *want {
                bail!("rejected {name}: it is not what it is listed as");
            }
            bail!("rejected {name}: its {len} bytes hash to {got}, not to {want}");
        }
    }

    /// Appends what the object has after the bytes of `partial`.
    async fn append_rest(
        &self,
        name: &str,
        partial: &mut Partial,
        received: &mut u64,
    ) -> anyhow::Result<()> {
        let Some(mut body) = self.read_from(name, partial.len).await? else {
            return Err(Missing(name.to_owned()).into());
        };
        loop {
            let chunk = tokio::time::timeout(self.patience, body.next()).await;
            let chunk = chunk.with_context(|| format!("no bytes for {:?}", self.patience))??;
            let Some(chunk) = chunk else { return Ok(()) };
            self.traffic.received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            *received += chunk.len() as u64;
            partial.append(&chunk).await?;
        }
    }
}

/// An object that a list names and the remote does not have.
#[derive(Debug)]
pub(crate) struct Missing(pub(super) String);

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is listed and is not there", self.0)
    }
}

impl std::error::Error for Missing {}

/// A request the service answered with a no that asking again does not change.
#[derive(Debug)]
pub(super) struct Refused(pub(super) String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}
