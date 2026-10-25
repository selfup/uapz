//! File batches held for a human decision, with an unchanged-file check before applying.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use crate::{node_data::WriteMode, storage, validate};

const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_DIFF_BYTES: usize = 1024 * 1024;
pub const REJECTED: &str = "the file changes were rejected; nothing in this batch was written";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Apply,
    Reject,
}

#[derive(Debug)]
pub struct FileReview {
    pub path: String,
    pub before_bytes: Option<usize>,
    pub after_bytes: usize,
    pub diff: String,
}

#[derive(Debug)]
pub struct Request {
    pub node: String,
    pub files: Vec<FileReview>,
}

pub type Handler = Arc<dyn Fn(Arc<Request>) -> Result<Decision, String> + Send + Sync>;

/// A one-shot decision channel. Dropping the UI or cancelling the run wakes the worker.
#[derive(Clone)]
pub struct Pending {
    pub request: Arc<Request>,
    reply: mpsc::SyncSender<Decision>,
}

impl Pending {
    pub fn decide(&self, decision: Decision) -> bool {
        self.reply.try_send(decision).is_ok()
    }
}

pub fn wait(
    request: Arc<Request>,
    cancel: &AtomicBool,
    publish: impl FnOnce(Pending),
) -> Result<Decision, String> {
    let (reply, rx) = mpsc::sync_channel(1);
    publish(Pending { request, reply });
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(crate::exec::CANCELLED.into());
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(decision) => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(crate::exec::CANCELLED.into());
                }
                return Ok(decision);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the file review was closed".into());
            }
        }
    }
}

struct Snapshot {
    relative: String,
    path: PathBuf,
    before: Option<Vec<u8>>,
}

pub struct Batch {
    pub request: Arc<Request>,
    snapshots: Vec<Snapshot>,
}

fn current(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(_) => storage::read(path, limit as u64).map(Some),
    }
}

impl Batch {
    /// Read the original destinations and construct the entire review without creating
    /// directories or files. Refuse an oversized diff rather than hiding changed lines.
    pub fn prepare(
        root: &Path,
        node: &str,
        files: &[(&str, &Path, &[u8])],
        mode: WriteMode,
    ) -> Result<Self, String> {
        let mut snapshots = Vec::new();
        let mut reviews = Vec::new();
        let mut remaining = MAX_SNAPSHOT_BYTES;
        let mut diff_remaining = MAX_DIFF_BYTES;
        for &(relative, path, bytes) in files {
            if validate::resolve_writable_inside(root, relative)? != path {
                return Err(format!(
                    "{relative}: destination changed while preparing review"
                ));
            }
            let before = current(path, remaining)?;
            let old = before.as_deref().unwrap_or_default();
            remaining = remaining.saturating_sub(old.len());
            if mode == WriteMode::Create && before.is_some() {
                return Err(format!("{relative} already exists; nothing was written"));
            }
            let appended;
            let after = if mode == WriteMode::Append {
                if old.len().saturating_add(bytes.len()) > MAX_SNAPSHOT_BYTES {
                    return Err(format!(
                        "{relative}: appended file exceeds the 64 MB review limit"
                    ));
                }
                appended = [old, bytes].concat();
                appended.as_slice()
            } else {
                bytes
            };
            let diff = diff(old, after, diff_remaining)?;
            diff_remaining -= diff.len();
            reviews.push(FileReview {
                path: relative.to_string(),
                before_bytes: before.as_ref().map(Vec::len),
                after_bytes: after.len(),
                diff,
            });
            snapshots.push(Snapshot {
                relative: relative.to_string(),
                path: path.to_path_buf(),
                before,
            });
        }
        Ok(Self {
            request: Arc::new(Request {
                node: node.to_string(),
                files: reviews,
            }),
            snapshots,
        })
    }

    /// Check the whole batch before the first write, including newly appeared files,
    /// deleted files and symlink retargeting while the review was open.
    pub fn verify(&self, root: &Path) -> Result<(), String> {
        for snapshot in &self.snapshots {
            let unchanged = || -> Result<bool, String> {
                let resolved = validate::resolve_writable_inside(root, &snapshot.relative)?;
                if resolved != snapshot.path {
                    return Ok(false);
                }
                Ok(current(&resolved, MAX_SNAPSHOT_BYTES)? == snapshot.before)
            };
            if !unchanged().unwrap_or(false) {
                return Err(format!(
                    "{} changed since review; nothing in this batch was written. Run again to review the new diff",
                    snapshot.relative
                ));
            }
        }
        Ok(())
    }
}

/// A linear-time unified diff, with three context lines around the changed region.
/// Binary changes are identified explicitly. Bound output before copying each line.
fn diff(before: &[u8], after: &[u8], limit: usize) -> Result<String, String> {
    let mut out = String::new();
    let mut push = |prefix: &str, line: &str| -> Result<(), String> {
        if out
            .len()
            .saturating_add(prefix.len())
            .saturating_add(line.len())
            .saturating_add(1)
            > limit
        {
            return Err(
                "the diff exceeds the 1 MB review limit; split the changes into smaller batches"
                    .into(),
            );
        }
        out.push_str(prefix);
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n');
        }
        Ok(())
    };
    if before == after {
        push("", "No content changes")?;
    } else if let (Ok(old), Ok(new)) = (std::str::from_utf8(before), std::str::from_utf8(after))
        && !old.contains('\0')
        && !new.contains('\0')
    {
        let old_len = old.split_inclusive('\n').count();
        let new_len = new.split_inclusive('\n').count();
        let prefix = old
            .split_inclusive('\n')
            .zip(new.split_inclusive('\n'))
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = old
            .split_inclusive('\n')
            .rev()
            .zip(new.split_inclusive('\n').rev())
            .take(old_len.min(new_len) - prefix)
            .take_while(|(a, b)| a == b)
            .count();
        let start = prefix.saturating_sub(3);
        let tail = suffix.min(3);
        let old_count = old_len - suffix + tail - start;
        let new_count = new_len - suffix + tail - start;
        push(
            "",
            &format!(
                "@@ -{},{} +{},{} @@",
                if old_count == 0 { start } else { start + 1 },
                old_count,
                if new_count == 0 { start } else { start + 1 },
                new_count
            ),
        )?;
        for line in old.split_inclusive('\n').skip(start).take(prefix - start) {
            push(" ", line)?;
        }
        for line in old
            .split_inclusive('\n')
            .skip(prefix)
            .take(old_len - suffix - prefix)
        {
            push("-", line)?;
            if !line.ends_with('\n') {
                push("", "\\ No newline at end of file")?;
            }
        }
        for line in new
            .split_inclusive('\n')
            .skip(prefix)
            .take(new_len - suffix - prefix)
        {
            push("+", line)?;
            if !line.ends_with('\n') {
                push("", "\\ No newline at end of file")?;
            }
        }
        for line in new.split_inclusive('\n').skip(new_len - suffix).take(tail) {
            push(" ", line)?;
        }
    } else {
        push(
            "",
            &format!(
                "Binary content changed: {} → {} bytes. No text diff is available.",
                before.len(),
                after.len()
            ),
        )?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("uapz-review-{tag}-{}-{stamp}", std::process::id()));
        fs::create_dir(&path).unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn diffs_show_edits_newlines_binary_changes_and_refuse_truncation() {
        assert!(
            diff(b"", b"new\n", 1024)
                .unwrap()
                .starts_with("@@ -0,0 +1,1 @@")
        );
        assert!(
            diff(b"old\n", b"", 1024)
                .unwrap()
                .starts_with("@@ -1,1 +0,0 @@")
        );
        let changed = diff(b"keep\nold\ntail\n", b"keep\nnew\ntail\n", 1024).unwrap();
        assert!(changed.contains(" keep\n-old\n+new\n tail\n"), "{changed}");
        let newline = diff(b"x", b"x\n", 1024).unwrap();
        assert!(newline.contains("No newline at end of file"));
        assert!(newline.contains("-x\n") && newline.contains("+x\n"));
        assert!(
            diff(&[0, 1], &[0, 2], 1024)
                .unwrap()
                .contains("Binary content changed")
        );
        assert!(diff(b"", b"new", 2).unwrap_err().contains("review limit"));
        assert_eq!(
            diff(b"same", b"same", 1024).unwrap(),
            "No content changes\n"
        );
    }

    #[test]
    fn preparing_a_review_never_creates_directories_or_files() {
        let root = scratch("no-effects");
        let target = root.join("new/path.txt");
        let batch = Batch::prepare(
            &root,
            "n1",
            &[("new/path.txt", &target, b"hello\n")],
            WriteMode::Create,
        )
        .unwrap();
        assert_eq!(batch.request.files[0].before_bytes, None);
        assert_eq!(batch.request.files[0].after_bytes, 6);
        assert!(batch.request.files[0].diff.contains("+hello"));
        assert!(!root.join("new").exists());
        batch.verify(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn append_preview_includes_original_content_and_checks_for_changes() {
        let root = scratch("append");
        let target = root.join("a.txt");
        fs::write(&target, "first\n").unwrap();
        let batch = Batch::prepare(
            &root,
            "n1",
            &[("a.txt", &target, b"second\n")],
            WriteMode::Append,
        )
        .unwrap();
        assert_eq!(batch.request.files[0].before_bytes, Some(6));
        assert_eq!(batch.request.files[0].after_bytes, 13);
        assert!(batch.request.files[0].diff.contains(" first\n+second\n"));
        batch.verify(&root).unwrap();
        fs::write(&target, "changed\n").unwrap();
        assert!(
            batch
                .verify(&root)
                .unwrap_err()
                .contains("changed since review")
        );
        fs::remove_file(&target).unwrap();
        assert!(batch.verify(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_new_file_appearing_invalidates_the_whole_review() {
        let root = scratch("appeared");
        let target = root.join("a.txt");
        let batch = Batch::prepare(
            &root,
            "n1",
            &[("a.txt", &target, b"new")],
            WriteMode::Overwrite,
        )
        .unwrap();
        fs::write(&target, "someone else's work").unwrap();
        assert!(batch.verify(&root).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "someone else's work");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retargeted_symlinks_invalidate_review_even_when_contents_match() {
        use std::os::unix::fs::symlink;
        let root = scratch("symlink");
        fs::write(root.join("a.txt"), "same").unwrap();
        fs::write(root.join("b.txt"), "same").unwrap();
        symlink(root.join("a.txt"), root.join("alias.txt")).unwrap();
        let batch = Batch::prepare(
            &root,
            "n1",
            &[("alias.txt", &root.join("a.txt"), b"new")],
            WriteMode::Overwrite,
        )
        .unwrap();
        fs::remove_file(root.join("alias.txt")).unwrap();
        symlink(root.join("b.txt"), root.join("alias.txt")).unwrap();
        assert!(batch.verify(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pending_review_can_be_approved_rejected_or_closed() {
        for decision in [Decision::Apply, Decision::Reject] {
            let request = Arc::new(Request {
                node: "n1".into(),
                files: Vec::new(),
            });
            assert_eq!(
                wait(request, &AtomicBool::new(false), |pending| {
                    assert!(pending.decide(decision));
                })
                .unwrap(),
                decision
            );
        }
        let request = Arc::new(Request {
            node: "n1".into(),
            files: Vec::new(),
        });
        assert!(
            wait(request, &AtomicBool::new(false), drop)
                .unwrap_err()
                .contains("closed")
        );
    }

    #[test]
    fn cancellation_wakes_a_waiting_review_and_wins_over_approval() {
        let request = Arc::new(Request {
            node: "n1".into(),
            files: Vec::new(),
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (tx, rx) = mpsc::channel();
        let worker =
            std::thread::spawn(move || wait(request, &flag, |pending| tx.send(pending).unwrap()));
        let _pending = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(worker.join().unwrap().unwrap_err(), crate::exec::CANCELLED);

        cancel.store(false, Ordering::Relaxed);
        let request = Arc::new(Request {
            node: "n1".into(),
            files: Vec::new(),
        });
        let error = wait(request, &cancel, |pending| {
            assert!(pending.decide(Decision::Apply));
            cancel.store(true, Ordering::Relaxed);
        })
        .unwrap_err();
        assert_eq!(error, crate::exec::CANCELLED);
    }
}
