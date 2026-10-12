//! Sealing secrets that must be stored but never shown again, such as a Discord bot token. This is envelope encryption, the same shape
//! cloud key services use: every secret gets its own random data key and is encrypted with it (AES-256-GCM); the data key is then
//! wrapped by a key-encryption key (KEK) that lives somewhere else. Rotating the KEK means re-wrapping the small data keys, never
//! asking every user for their token again, and moving the KEK into a cloud KMS or Vault changes only a `KeyProvider`.
//!
//! A sealed secret is bound to its owner (the tenant and a label such as the bot id) as associated data, so a sealed value copied
//! from one account's row into another's fails to open instead of working.
//!
//! Sealed text looks like `v1.<kek id>.<wrapped data key>.<ciphertext>`, each part base64.

use aes_gcm::{
    Aes256Gcm, Key, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};

/// Where the key-encryption keys live. The file-backed one below is for a single small server; a KMS or Vault client would be another
/// implementation of this and nothing else would change.
pub trait KeyProvider: Send + Sync {
    /// The id of the key new secrets are wrapped with.
    fn current(&self) -> String;
    /// Wraps a data key with the key of this id. None if the id is unknown.
    fn wrap(&self, key_id: &str, data_key: &[u8; 32], aad: &[u8]) -> Option<Vec<u8>>;
    /// Opens a wrapped data key. None if the id is unknown or the wrapped key was changed.
    fn unwrap(&self, key_id: &str, wrapped: &[u8], aad: &[u8]) -> Option<[u8; 32]>;
}

/// Key-encryption keys held in memory, loaded from a file only the server's user can read. The first key is current; older ones stay
/// so secrets sealed under them can still be opened until they are re-wrapped.
pub struct LocalKeys {
    keys: Vec<(String, [u8; 32])>,
}

impl LocalKeys {
    /// Keys from a file with one `id:hex` line each (64 hex characters of key), newest first. If the file does not exist it is made
    /// with one new random key, readable only by its owner. Back this file up: without it every sealed token is lost.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self, String> {
        if !path.exists() {
            let mut k = [0u8; 32];
            getrandom::fill(&mut k).map_err(|e| e.to_string())?;
            let line = format!("k1:{}\n", hex(&k));
            std::fs::write(path, line).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| e.to_string())?;
            }
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Keys from the text of a key file.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut keys = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (id, h) = line
                .split_once(':')
                .ok_or("a key line must look like id:64 hex characters")?;
            let bytes = unhex(h).ok_or("a key must be 64 hex characters")?;
            let key: [u8; 32] = bytes.try_into().map_err(|_| "a key must be 32 bytes")?;
            if id.is_empty() || id.contains('.') {
                return Err("a key id must not be empty or contain a dot".into());
            }
            keys.push((id.to_string(), key));
        }
        if keys.is_empty() {
            return Err("the key file has no keys".into());
        }
        Ok(Self { keys })
    }

    /// A random key set for tests.
    pub fn random() -> Self {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).expect("the system has a random source");
        Self {
            keys: vec![("k1".into(), k)],
        }
    }

    /// The same keys with a new current key in front, for rotation.
    pub fn rotated(mut self) -> Self {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).expect("the system has a random source");
        let id = format!("k{}", self.keys.len() + 1);
        self.keys.insert(0, (id, k));
        self
    }

    fn key(&self, id: &str) -> Option<&[u8; 32]> {
        self.keys.iter().find(|(i, _)| i == id).map(|(_, k)| k)
    }
}

impl KeyProvider for LocalKeys {
    fn current(&self) -> String {
        self.keys[0].0.clone()
    }

    fn wrap(&self, key_id: &str, data_key: &[u8; 32], aad: &[u8]) -> Option<Vec<u8>> {
        encrypt(self.key(key_id)?, data_key, aad)
    }

    fn unwrap(&self, key_id: &str, wrapped: &[u8], aad: &[u8]) -> Option<[u8; 32]> {
        decrypt(self.key(key_id)?, wrapped, aad)?.try_into().ok()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// AES-256-GCM with a fresh random nonce put in front of the ciphertext.
fn encrypt(key: &[u8; 32], plain: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).ok()?;
    let ct = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: plain, aad })
        .ok()?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Some(out)
}

/// The reverse of `encrypt`. None if anything, including the associated data, differs.
fn decrypt(key: &[u8; 32], sealed: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 12 + 16 {
        return None;
    }
    let (nonce, ct) = sealed.split_at(12);
    Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key))
        .decrypt(&Nonce::try_from(nonce).ok()?, Payload { msg: ct, aad })
        .ok()
}

/// The associated data that ties a sealed secret to its owner.
fn aad(owner: &str, label: &str) -> Vec<u8> {
    format!("claudecord:{owner}:{label}").into_bytes()
}

/// Seals a secret for an owner (a tenant) and label (for example a bot's application id).
pub fn seal(keys: &dyn KeyProvider, owner: &str, label: &str, secret: &str) -> Option<String> {
    let mut data_key = [0u8; 32];
    getrandom::fill(&mut data_key).ok()?;
    let a = aad(owner, label);
    let id = keys.current();
    let wrapped = keys.wrap(&id, &data_key, &a)?;
    let body = encrypt(&data_key, secret.as_bytes(), &a)?;
    Some(format!(
        "v1.{id}.{}.{}",
        B64.encode(wrapped),
        B64.encode(body)
    ))
}

/// Opens a sealed secret. None if it was changed, belongs to another owner or label, or its key is gone.
pub fn open(keys: &dyn KeyProvider, owner: &str, label: &str, sealed: &str) -> Option<String> {
    let mut p = sealed.split('.');
    let (v, id, wrapped, body) = (p.next()?, p.next()?, p.next()?, p.next()?);
    if v != "v1" || p.next().is_some() {
        return None;
    }
    let a = aad(owner, label);
    let data_key = keys.unwrap(id, &B64.decode(wrapped).ok()?, &a)?;
    String::from_utf8(decrypt(&data_key, &B64.decode(body).ok()?, &a)?).ok()
}

/// Re-wraps a sealed secret's data key under the current key, leaving the encrypted body alone. None if it cannot be opened.
pub fn rewrap(keys: &dyn KeyProvider, owner: &str, label: &str, sealed: &str) -> Option<String> {
    let mut p = sealed.split('.');
    let (v, id, wrapped, body) = (p.next()?, p.next()?, p.next()?, p.next()?);
    if v != "v1" || p.next().is_some() {
        return None;
    }
    let a = aad(owner, label);
    let data_key = keys.unwrap(id, &B64.decode(wrapped).ok()?, &a)?;
    let now = keys.current();
    let wrapped = keys.wrap(&now, &data_key, &a)?;
    Some(format!("v1.{now}.{}.{body}", B64.encode(wrapped)))
}

/// Whether text looks like a sealed secret, so a listing can refuse to ever return one.
pub fn looks_sealed(s: &str) -> bool {
    s.starts_with("v1.") && s.matches('.').count() == 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_binding() {
        let k = LocalKeys::random();
        let s = seal(&k, "t1", "bot1", "SECRET").unwrap();
        assert!(!s.contains("SECRET"));
        assert_eq!(open(&k, "t1", "bot1", &s).as_deref(), Some("SECRET"));
        // Another owner or label cannot open it.
        assert_eq!(open(&k, "t2", "bot1", &s), None);
        assert_eq!(open(&k, "t1", "bot2", &s), None);
    }

    #[test]
    fn tampering_fails() {
        let k = LocalKeys::random();
        let s = seal(&k, "t1", "b", "SECRET").unwrap();
        let mut bad = s.clone();
        let last = bad.pop().unwrap();
        bad.push(if last == 'A' { 'B' } else { 'A' });
        assert_eq!(open(&k, "t1", "b", &bad), None);
        assert_eq!(open(&LocalKeys::random(), "t1", "b", &s), None);
    }

    #[test]
    fn rotation_rewraps_without_the_secret() {
        let k = LocalKeys::random();
        let old = seal(&k, "t1", "b", "SECRET").unwrap();
        let k = k.rotated();
        // The old secret still opens under the new key set, and after a rewrap it names the new key.
        assert_eq!(open(&k, "t1", "b", &old).as_deref(), Some("SECRET"));
        let new = rewrap(&k, "t1", "b", &old).unwrap();
        assert!(new.starts_with("v1.k2."));
        assert_eq!(open(&k, "t1", "b", &new).as_deref(), Some("SECRET"));
        assert!(looks_sealed(&new));
    }

    #[test]
    fn key_file_is_made_and_reread() {
        let dir = std::env::temp_dir().join(format!("cc-seal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("kek");
        let _ = std::fs::remove_file(&f);
        let a = LocalKeys::load_or_create(&f).unwrap();
        let s = seal(&a, "t", "l", "x").unwrap();
        let b = LocalKeys::load_or_create(&f).unwrap();
        assert_eq!(open(&b, "t", "l", &s).as_deref(), Some("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
