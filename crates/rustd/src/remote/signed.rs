//! Who a list on the remote is from.
//!
//! Everything a machine takes from a remote is named in a list by the hash of its bytes, so
//! the lists are the one thing a remote could change without being caught by a hash. A
//! machine that pushes signs its lists with an Ed25519 key, and a machine that names the
//! keys it trusts takes no list that none of them signed.

use std::path::Path;

use anyhow::{Context, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// The word the last line of a signed list starts with.
const SIGNED: &str = "signed";

/// The keys of this machine: `RUSTD_REMOTE_SIGN` names the file of the key it signs with,
/// `RUSTD_REMOTE_TRUST` lists the public keys it takes lists from, with commas between.
pub(crate) struct Trust {
    signer: Option<SigningKey>,
    trusted: Vec<VerifyingKey>,
}

impl Trust {
    pub(crate) fn from_env() -> anyhow::Result<Trust> {
        let signer = match std::env::var_os("RUSTD_REMOTE_SIGN") {
            Some(file) => Some(read_key(Path::new(&file))?),
            None => None,
        };
        let trusted = std::env::var("RUSTD_REMOTE_TRUST").unwrap_or_default();
        let trusted = trusted.split(',').filter(|it| !it.is_empty()).map(|key| {
            let bytes = unhex::<32>(key).context("RUSTD_REMOTE_TRUST holds what is not a key")?;
            VerifyingKey::from_bytes(&bytes).context("RUSTD_REMOTE_TRUST holds what is not a key")
        });
        Ok(Trust { signer, trusted: trusted.collect::<anyhow::Result<_>>()? })
    }

    /// Whether lists are checked at all.
    pub(crate) fn checks(&self) -> bool {
        !self.trusted.is_empty()
    }

    /// Why this machine may not write lists, if it may not: other machines that trust as
    /// this one does would take nothing from an unsigned list.
    pub(crate) fn cannot_sign(&self) -> Option<&'static str> {
        (self.checks() && self.signer.is_none())
            .then_some("RUSTD_REMOTE_TRUST is set and RUSTD_REMOTE_SIGN is not")
    }

    /// `body` as the object `name` holds it: with the signature of this machine's key as
    /// its last line, when it has one.
    pub(crate) fn seal(&self, name: &str, body: &str) -> String {
        let Some(signer) = &self.signer else { return body.to_owned() };
        let signature = signer.sign(&signed_bytes(name, body));
        let (key, signature) = (hex(signer.verifying_key().as_bytes()), hex(&signature.to_bytes()));
        format!("{body}{SIGNED} {key} {signature}\n")
    }

    /// The lines of the list in `text`, the object `name`, for a machine that reads it.
    /// With keys to trust, a list that none of them signed for this name is an error.
    pub(crate) fn open<'a>(&self, name: &str, text: &'a str) -> anyhow::Result<&'a str> {
        if self.checks() { self.verify(name, text) } else { Ok(split(text).0) }
    }

    /// The lines of the list, for a machine that adds to it and writes it back. One that
    /// signs puts its name to every line, so it takes none it would not have trusted.
    pub(crate) fn open_to_extend<'a>(&self, name: &str, text: &'a str) -> anyhow::Result<&'a str> {
        if self.signer.is_some() { self.verify(name, text) } else { self.open(name, text) }
    }

    /// The lines of a list that a trusted key, or this machine's own, signed for `name`.
    fn verify<'a>(&self, name: &str, text: &'a str) -> anyhow::Result<&'a str> {
        let (body, signature) = split(text);
        let mut fields = signature.split(' ').skip(1);
        let key = fields.next().and_then(unhex::<32>);
        let signature = fields.next().and_then(unhex::<64>);
        let (Some(key), Some(signature)) = (key, signature) else {
            bail!("`{name}` is not signed");
        };
        let own = self.signer.as_ref().map(|it| it.verifying_key());
        let Some(key) = self.trusted.iter().chain(&own).find(|it| *it.as_bytes() == key) else {
            bail!("`{name}` is signed by {}, which is not a key this machine trusts", hex(&key));
        };
        key.verify_strict(&signed_bytes(name, body), &Signature::from_bytes(&signature))
            .ok()
            .with_context(|| format!("`{name}` is not what {} signed", hex(key.as_bytes())))?;
        Ok(body)
    }
}

/// The lines of a list and the line of its signature, which is empty when it has none.
fn split(text: &str) -> (&str, &str) {
    match text.trim_end_matches('\n').rsplit_once('\n') {
        Some((body, last)) if last.starts_with(SIGNED) => (&text[..body.len() + 1], last),
        _ if text.starts_with(SIGNED) => ("", text.trim_end_matches('\n')),
        _ => (text, ""),
    }
}

/// `rustd cache keygen FILE`: writes a new key to sign with and prints the public one.
pub(crate) fn keygen(file: &Path) -> anyhow::Result<()> {
    let mut seed = [0; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no random bytes: {e}"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut to = options.open(file).with_context(|| format!("writing {}", file.display()))?;
    std::io::Write::write_all(&mut to, format!("{}\n", hex(&seed)).as_bytes())?;
    println!("{}", hex(SigningKey::from_bytes(&seed).verifying_key().as_bytes()));
    Ok(())
}

fn read_key(file: &Path) -> anyhow::Result<SigningKey> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("reading the key in {}", file.display()))?;
    let seed = unhex::<32>(text.trim())
        .with_context(|| format!("{} does not hold a key", file.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// What a signature is over: the name too, so that a list signed for one project is not
/// one for another.
fn signed_bytes(name: &str, body: &str) -> Vec<u8> {
    format!("rustd list 1\n{name}\n{body}").into_bytes()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn trusting(signer: Option<u8>, trusted: &[u8]) -> Trust {
        Trust {
            signer: signer.map(key),
            trusted: trusted.iter().map(|&it| key(it).verifying_key()).collect(),
        }
    }

    #[test]
    fn a_list_is_taken_from_a_trusted_key_only() {
        let sealed = trusting(Some(1), &[]).seal("manifests/a", "one\ntwo\n");
        assert_eq!(trusting(None, &[1]).open("manifests/a", &sealed).unwrap(), "one\ntwo\n");
        assert_eq!(trusting(None, &[]).open("manifests/a", &sealed).unwrap(), "one\ntwo\n");
        assert!(trusting(None, &[2]).open("manifests/a", &sealed).is_err());
        assert!(trusting(Some(2), &[]).open_to_extend("manifests/a", &sealed).is_err());
        assert!(trusting(Some(1), &[]).open_to_extend("manifests/a", &sealed).is_ok());
    }

    #[test]
    fn a_list_that_was_changed_or_moved_is_refused() {
        let sealed = trusting(Some(1), &[]).seal("manifests/a", "one\ntwo\n");
        let trust = trusting(None, &[1]);
        assert!(trust.open("manifests/b", &sealed).is_err());
        assert!(trust.open("manifests/a", &sealed.replace("two", "six")).is_err());
        assert!(trust.open("manifests/a", &sealed.replacen("one\n", "", 1)).is_err());
        assert!(trust.open("manifests/a", "one\ntwo\n").is_err());
        assert!(trust.open("manifests/a", "").is_err());
    }

    #[test]
    fn an_empty_list_is_signed_as_any_other() {
        let sealed = trusting(Some(1), &[]).seal("manifests/a", "");
        assert_eq!(trusting(None, &[1]).open("manifests/a", &sealed).unwrap(), "");
    }
}
