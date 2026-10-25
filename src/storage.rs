//! Bounded file reads and writes that never reuse a predictable temporary file.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Graphs, presets and history records are documents, not arbitrary data streams.
pub const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

/// Walk managed directories beneath a trusted root, refusing symlinked components.
/// In particular, listing or clearing cache/history must not operate on a linked tree.
pub fn directory(root: &Path, relative: &Path, create: bool) -> Result<PathBuf, String> {
    let mut path = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    for part in relative.components() {
        let std::path::Component::Normal(name) = part else {
            return Err("managed directory must be a plain relative path".into());
        };
        path.push(name);
        if create {
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("create {}: {e}", path.display())),
            }
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !meta.is_dir() {
            return Err(format!(
                "{} is not an ordinary directory (symlinks are refused)",
                path.display()
            ));
        }
    }
    Ok(path)
}

pub fn check_size(bytes: usize, limit: u64) -> Result<(), String> {
    if bytes as u64 > limit {
        Err(format!("file is over the {limit}-byte limit"))
    } else {
        Ok(())
    }
}

/// Read an ordinary file with a limit on the actual read, not just the metadata seen
/// before it. Callers that permit symlinks must first resolve and validate their target.
pub fn read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > limit {
        return Err(format!("{} is over the {limit}-byte limit", path.display()));
    }
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() > limit {
        return Err(format!(
            "{} is not a regular file within the size limit",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    check_size(bytes.len(), limit).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(bytes)
}

pub fn read_text(path: &Path, limit: u64) -> Result<String, String> {
    String::from_utf8(read(path, limit)?)
        .map_err(|_| format!("{} is not valid UTF-8", path.display()))
}

/// Use exclusive creation for each staging file. Concurrent writers get independent
/// files, and a planted symlink is never followed. A rename publishes complete bytes.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut staging = None;
    for _ in 0..128 {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("{}-{nonce}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => {
                staging = Some((tmp, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create {}: {e}", tmp.display())),
        }
    }
    let (tmp, mut file) = staging.ok_or("could not reserve a temporary file")?;
    let result = file
        .write_all(bytes)
        .map_err(|e| format!("write {}: {e}", tmp.display()));
    drop(file);
    let result = result.and_then(|()| {
        fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Save a new timestamped record without clobbering another writer's record. Names and
/// extensions are caller-controlled safe stems, never paths supplied by model output.
pub fn write_unique(
    dir: &Path,
    stem: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    for n in 0..10_000 {
        let name = if n == 0 {
            stem.to_string()
        } else {
            format!("{stem}-{n}")
        };
        let path = dir.join(format!("{name}.{extension}"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(bytes) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(format!("write {}: {e}", path.display()));
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create {}: {e}", path.display())),
        }
    }
    Err("too many files with the same timestamp".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn scratch() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "uapz-storage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn concurrent_writers_publish_whole_files_and_keep_distinct_records() {
        let root = scratch();
        let barrier = Arc::new(Barrier::new(8));
        let writers: Vec<_> = (0..8u8)
            .map(|n| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let bytes = vec![n; 64 * 1024];
                    barrier.wait();
                    write_atomic(&root.join("shared.bin"), &bytes).unwrap();
                    let path = write_unique(&root, "same-time", "bin", &bytes).unwrap();
                    assert_eq!(read(&path, bytes.len() as u64).unwrap(), bytes);
                    path
                })
            })
            .collect();
        let paths: std::collections::HashSet<_> =
            writers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(paths.len(), 8);
        let bytes = read(&root.join("shared.bin"), 64 * 1024).unwrap();
        assert_eq!(bytes.len(), 64 * 1024);
        assert!(bytes.iter().all(|b| *b == bytes[0]));
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            9,
            "staging files were left behind"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_reads_reject_large_and_non_regular_files() {
        let root = scratch();
        let path = root.join("file");
        fs::write(&path, b"1234").unwrap();
        assert_eq!(read(&path, 4).unwrap(), b"1234");
        assert!(read(&path, 3).unwrap_err().contains("limit"));
        assert!(read(&root, 4).unwrap_err().contains("regular file"));
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(read(&link, 4).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
