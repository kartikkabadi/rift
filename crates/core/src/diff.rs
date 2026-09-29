use crate::{Error, Result, marker, strategy::portable};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// How one path differs between the two sides of a diff.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// Present in `to`, missing in `from`.
    Added,
    /// Present in `from`, missing in `to`.
    Removed,
    /// Present in both but not identical.
    Changed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiffEntry {
    /// Path relative to the compared roots.
    pub path: PathBuf,
    pub kind: DiffKind,
}

/// The file-level changes that turn `from` into `to`.
#[derive(Clone, Debug, Serialize)]
pub struct TreeDiff {
    pub from: PathBuf,
    pub to: PathBuf,
    pub entries: Vec<DiffEntry>,
}

impl TreeDiff {
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The changes `to` made relative to `from`: added in `to`, removed from
/// `from`, or differing between them.
pub(crate) fn diff_trees(from: &Path, to: &Path) -> Result<TreeDiff> {
    let left = manifest(from)?;
    let right = manifest(to)?;
    let mut entries = Vec::new();
    for (path, entry) in &left {
        match right.get(path) {
            None => entries.push(DiffEntry {
                path: path.clone(),
                kind: DiffKind::Removed,
            }),
            Some(other) if entry != other => entries.push(DiffEntry {
                path: path.clone(),
                kind: DiffKind::Changed,
            }),
            Some(_) => {}
        }
    }
    for path in right.keys() {
        if !left.contains_key(path) {
            entries.push(DiffEntry {
                path: path.clone(),
                kind: DiffKind::Added,
            });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(TreeDiff {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        entries,
    })
}

/// Replays a diff: conforms `diff.from` to `diff.to`. Added and changed
/// entries are copied from `to`; removed entries are deleted from `from`.
/// Each entry carries the source's metadata just like a regular copy.
pub(crate) fn apply_diff(diff: &TreeDiff) -> Result<()> {
    let mut directories = BTreeSet::new();
    for entry in &diff.entries {
        let source = diff.to.join(&entry.path);
        let destination = diff.from.join(&entry.path);
        if entry.kind == DiffKind::Removed {
            remove_path(&destination)?;
            continue;
        }
        let metadata = fs::symlink_metadata(&source)?;
        let file_type = metadata.file_type();
        let existing = fs::symlink_metadata(&destination)
            .ok()
            .map(|metadata| metadata.file_type());
        let same_kind = existing.is_some_and(|existing| {
            (existing.is_dir() && file_type.is_dir())
                || (existing.is_file() && file_type.is_file())
                || (existing.is_symlink() && file_type.is_symlink())
        });
        if !same_kind {
            remove_path(&destination)?;
        }
        if file_type.is_dir() {
            if existing.is_none() || !same_kind {
                fs::create_dir(&destination)?;
            }
            directories.insert(destination.clone());
        } else if file_type.is_file() {
            fs::copy(&source, &destination)?;
            portable::copy_metadata(
                &source,
                &destination,
                portable::MetadataTarget::FileOrDirectory,
            )?;
        } else if file_type.is_symlink() {
            if existing.is_some() {
                remove_path(&destination)?;
            }
            portable::create_symlink(&source, &destination)?;
            portable::copy_metadata(&source, &destination, portable::MetadataTarget::Symlink)?;
        } else {
            return Err(Error::UnsupportedEntry(source));
        }
        if let Some(parent) = destination.parent() {
            directories.insert(parent.to_path_buf());
        }
    }
    // Update directory metadata once their contents are settled, deepest
    // first so parent timestamps are written last.
    for destination in directories.iter().rev() {
        let source = diff.to.join(
            destination
                .strip_prefix(&diff.from)
                .map_err(|error| Error::Path(error.to_string()))?,
        );
        if fs::symlink_metadata(&source).is_ok_and(|m| m.is_dir()) {
            portable::copy_metadata(
                &source,
                destination,
                portable::MetadataTarget::FileOrDirectory,
            )?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() {
        remove_dir_all(path)
    } else {
        remove_file(path)
    }
}

/// `fs::remove_file`/`remove_dir_all` fail on Windows for read-only files
/// (common under `.git`), so a permission failure retries after clearing
/// the read-only bit.
fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let mut permissions = fs::metadata(path)?.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn remove_dir_all(path: &Path) -> Result<()> {
    // Read-only entries under the tree (git objects, caches) can block a
    // recursive delete on Windows; make everything writable first.
    let _ = make_writable(path);
    fs::remove_dir_all(path)?;
    Ok(())
}

fn make_writable(path: &Path) -> Result<()> {
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(entry.path(), permissions)?;
        }
    }
    Ok(())
}

/// A content fingerprint for one filesystem entry. Two entries with equal
/// fingerprints are treated as unchanged: files compare by size,
/// modification time, and mode; symlinks by target; directories by mode
/// only, since a directory's own mtime shifts whenever a child changes.
#[derive(Debug, Eq, PartialEq)]
struct Entry {
    kind: EntryKind,
    size: u64,
    modified_nanos: i64,
    mode: u32,
    link_target: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File,
    Symlink,
}

fn manifest(root: &Path) -> Result<BTreeMap<PathBuf, Entry>> {
    let marker = marker::path(root);
    let mut entries = BTreeMap::new();
    for entry in WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
    {
        let entry = entry?;
        let path = entry.path();
        if path == marker {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| Error::Path(error.to_string()))?;
        let metadata = fs::symlink_metadata(path)?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            return Err(Error::UnsupportedEntry(path.to_path_buf()));
        };
        let modified_nanos = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as i64)
            .unwrap_or(0);
        entries.insert(
            relative.to_path_buf(),
            Entry {
                kind,
                size: if file_type.is_file() {
                    metadata.len()
                } else {
                    0
                },
                modified_nanos: if kind == EntryKind::File {
                    modified_nanos
                } else {
                    0
                },
                mode: mode(&metadata),
                link_target: if file_type.is_symlink() {
                    Some(fs::read_link(path)?)
                } else {
                    None
                },
            },
        );
    }
    Ok(entries)
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode()
}

#[cfg(windows)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes()
}

#[cfg(not(any(unix, windows)))]
fn mode(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}
