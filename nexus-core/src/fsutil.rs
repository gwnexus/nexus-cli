//! File writes for credentials and state that must never be half written
//! or briefly readable by others.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `bytes` to `path` atomically with mode 0600 (Unix).
///
/// The content goes into a new temporary file next to `path` (created
/// exclusively, never following an existing path, owner-only before any
/// byte is written), is synced, and then renamed over `path`. An existing
/// file is replaced as a whole, so its old permissions do not carry over,
/// and a failed write leaves it untouched. The parent directory is created
/// when missing; its permissions are left to the caller. After the rename
/// the directory is synced (Unix) so the new entry survives a crash; that
/// sync is best effort, because the replacement has already happened.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir)?;
    let (tmp, mut file) = create_temp(dir, path)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            fs::rename(&tmp, path)
        });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    sync_dir(dir);
    Ok(())
}

/// Flush a directory entry change (rename, create) to disk.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// A new, exclusively created temp file in `dir` named after `target`.
fn create_temp(dir: &Path, target: &Path) -> std::io::Result<(PathBuf, fs::File)> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut last = None;
    for attempt in 0..16u32 {
        let tmp = dir.join(format!(
            ".{name}.{}.{nanos}.{attempt}.tmp",
            std::process::id()
        ));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no temporary file name available")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_writes_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/creds.toml");
        write_private_atomic(&path, b"token = 1\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "token = 1\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_replaces_a_readable_file_with_a_private_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // No temporary files are left behind.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_does_not_write_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        let path = dir.path().join("creds.toml");
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        write_private_atomic(&path, b"secret").unwrap();
        // The link is replaced by a regular file; its old target is untouched.
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert!(!fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&path).unwrap(), "secret");
    }

    #[cfg(unix)]
    #[test]
    fn test_failed_write_keeps_the_old_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.toml");
        fs::write(&path, "old").unwrap();
        // A read-only directory refuses the temporary file.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = write_private_atomic(&path, b"new");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        if result.is_ok() {
            // Running as root ignores directory permissions; nothing to check.
            return;
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    }
}
