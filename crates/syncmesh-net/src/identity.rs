//! Persistent Ed25519 identity for a syncmesh endpoint.
//!
//! The secret key is stored on disk as 32 raw bytes. Higher layers (the binary
//! crate) are responsible for picking the path — typically a file under the
//! platform-specific config dir. This module only deals with load/generate/save.

use std::io;
use std::path::Path;

use iroh::SecretKey;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("identity file has unexpected length {0}, expected 32 bytes")]
    BadLength(usize),
}

/// Loads a secret key from `path`, or generates a fresh one and writes it there.
///
/// The parent directory must already exist; callers that need to create it should
/// do so before calling. The file is written with best-effort restrictive
/// permissions on Unix (0600); on Windows, file ACLs follow the default for the
/// parent directory.
pub fn load_or_create(path: &Path) -> Result<SecretKey, IdentityError> {
    match std::fs::read(path) {
        Ok(bytes) => parse_secret(&bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            write_secret(path, &key)?;
            Ok(key)
        }
        Err(e) => Err(IdentityError::Io(e)),
    }
}

/// Generates a fresh in-memory identity. Used by tests and by first-run flows
/// where persistence isn't wanted yet.
pub fn generate() -> SecretKey {
    SecretKey::generate()
}

fn parse_secret(bytes: &[u8]) -> Result<SecretKey, IdentityError> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| IdentityError::BadLength(bytes.len()))?;
    Ok(SecretKey::from_bytes(&arr))
}

fn write_secret(path: &Path, key: &SecretKey) -> Result<(), IdentityError> {
    let bytes = key.to_bytes();
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_on_disk() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("id");
        let k1 = load_or_create(&p).unwrap();
        let k2 = load_or_create(&p).unwrap();
        assert_eq!(k1.to_bytes(), k2.to_bytes());
    }

    #[test]
    fn fresh_generates_new_each_time() {
        let a = generate();
        let b = generate();
        assert_ne!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn rejects_bad_length() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("id");
        std::fs::write(&p, b"too short").unwrap();
        assert!(matches!(
            load_or_create(&p),
            Err(IdentityError::BadLength(_))
        ));
    }
}
