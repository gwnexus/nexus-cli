//! Read-modify-write of the small JSON record files (the sync manifest and
//! the pull manifest) that several `nexus` processes may update at once.
//!
//! Each update holds an exclusive lock on a sidecar `<file>.lock`, re-reads
//! the file under it, applies the change and replaces the file atomically,
//! so concurrent runs do not overwrite each other's records. A file that
//! exists but cannot be read or parsed is an error: it is never replaced by
//! an empty record set.

use std::fs;
use std::path::{Path, PathBuf};

/// Apply `change` to the JSON object stored at `path` (missing: empty) and
/// write it back when it changed. Returns what `change` returned.
pub(crate) fn update_object<T>(
    path: &Path,
    change: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> T,
) -> anyhow::Result<T> {
    let _lock = FileLock::acquire(path)?;
    let before = read_object(path)?;
    let mut object = before.clone().unwrap_or_default();
    let result = change(&mut object);
    if before.as_ref() != Some(&object) {
        let content = serde_json::to_string_pretty(&object)? + "\n";
        nexus_core::fsutil::write_private_atomic(path, content.as_bytes())
            .map_err(|e| anyhow::anyhow!("could not write {}: {e}", path.display()))?;
    }
    Ok(result)
}

/// The JSON object at `path`: `None` when the file does not exist, an
/// error when it cannot be read or is not a JSON object.
pub(crate) fn read_object(
    path: &Path,
) -> anyhow::Result<Option<serde_json::Map<String, serde_json::Value>>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => anyhow::bail!("could not read {}: {e}", path.display()),
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Object(map)) => Ok(Some(map)),
        Ok(_) => anyhow::bail!("{} is not a JSON object; fix or remove it", path.display()),
        Err(e) => anyhow::bail!(
            "{} cannot be parsed ({e}); fix or remove it",
            path.display()
        ),
    }
}

/// An exclusive advisory lock on `<path>.lock`, released on drop.
struct FileLock {
    #[cfg(unix)]
    _file: fs::File,
}

impl FileLock {
    fn acquire(path: &Path) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::io::AsRawFd;
            let mut name = path.as_os_str().to_owned();
            name.push(".lock");
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(PathBuf::from(name))?;
            // Nonblocking with a bounded wait (about 5 s): callers run on
            // async threads, which must not block on another process.
            for _ in 0..100 {
                // SAFETY: the descriptor belongs to `file`, which the lock owns.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Ok(Self { _file: file });
                }
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(err.into());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            anyhow::bail!(
                "{} is locked by another nexus command; try again",
                path.display()
            )
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(Self {})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_object_creates_merges_and_skips_unchanged_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/m.json");
        update_object(&path, |m| m.insert("a".into(), 1.into())).unwrap();
        update_object(&path, |m| m.insert("b".into(), 2.into())).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({"a": 1, "b": 2}));
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        update_object(&path, |_| ()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn test_corrupt_file_is_an_error_and_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.json");
        fs::write(&path, "{ broken").unwrap();
        assert!(update_object(&path, |m| m.insert("a".into(), 1.into())).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ broken");
        fs::write(&path, "[]").unwrap();
        assert!(read_object(&path).is_err());
    }

    #[test]
    fn test_concurrent_updates_keep_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        update_object(&path, |m| m.insert(format!("{i}-{j}"), true.into()))
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(read_object(&path).unwrap().unwrap().len(), 80);
    }
}
