use std::{fs::OpenOptions, io::Read, path::Path};

use zeroize::Zeroizing;

const MAX_SECRET_FILE_BYTES: usize = 64 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretFileError {
    Missing,
    Read,
    TooLarge,
    NotRegular,
    InsecurePermissions,
    WrongOwner,
    Unsupported,
}

/// Loads a small owner-only secret from an opened file descriptor.
///
/// On Unix, `O_NOFOLLOW` prevents opening a symlink and all security checks use
/// metadata from that same descriptor, avoiding a path-stat/open TOCTOU race.
/// The effective uid check uses the POSIX process identity rather than the
/// caller's real uid, which also handles setuid deployments correctly.
pub fn load_owner_only_secret(
    path: &Path,
    max_bytes: usize,
) -> Result<Zeroizing<String>, SecretFileError> {
    let max_bytes = max_bytes.min(MAX_SECRET_FILE_BYTES);
    if max_bytes == 0 {
        return Err(SecretFileError::TooLarge);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let file = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    SecretFileError::Missing
                } else {
                    SecretFileError::Read
                }
            })?;
        let metadata = file.metadata().map_err(|_| SecretFileError::Read)?;
        if !metadata.is_file() {
            return Err(SecretFileError::NotRegular);
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(SecretFileError::InsecurePermissions);
        }
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(SecretFileError::WrongOwner);
        }
        if metadata.len() > max_bytes as u64 {
            return Err(SecretFileError::TooLarge);
        }

        let mut value = Zeroizing::new(String::new());
        file.take(max_bytes as u64 + 1)
            .read_to_string(&mut value)
            .map_err(|_| SecretFileError::Read)?;
        if value.len() > max_bytes {
            return Err(SecretFileError::TooLarge);
        }
        Ok(value)
    }

    #[cfg(not(unix))]
    {
        let _ = (path, max_bytes);
        Err(SecretFileError::Unsupported)
    }
}
#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn path(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("xiao-secret-file-{}-{suffix}", std::process::id()))
    }

    #[test]
    fn accepts_owner_only_regular_file() {
        let path = path("valid");
        fs::write(&path, "secret\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("permissions");
        assert_eq!(
            &*load_owner_only_secret(&path, 128).expect("load"),
            "secret\n"
        );
        fs::remove_file(path).expect("remove");
    }

    #[test]
    fn rejects_symlink_and_group_readable_file() {
        let target = path("target");
        let link = path("link");
        fs::write(&target, "secret").expect("write");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("permissions");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(matches!(
            load_owner_only_secret(&link, 128),
            Err(SecretFileError::Read)
        ));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).expect("permissions");
        assert_eq!(
            load_owner_only_secret(&target, 128),
            Err(SecretFileError::InsecurePermissions)
        );
        fs::remove_file(link).expect("remove");
        fs::remove_file(target).expect("remove");
    }
}
