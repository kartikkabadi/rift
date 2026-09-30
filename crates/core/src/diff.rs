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
/// `from`, or differing between them. `skip` hides relative paths on both
/// sides (the base manifest's excluded paths).
pub(crate) fn diff_trees(from: &Path, to: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<TreeDiff> {
    let left = manifest(from, skip)?;
    let right = manifest(to, skip)?;
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
        // The merge plan already refuses writes under a container `ours`
        // broke; this check is the enforcement at the write boundary so a
        // symlinked directory inside the destination can never redirect an
        // entry (or a removal) outside the workspace root.
        check_container_chain(&diff.from, &destination)?;
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
            // Overwriting a read-only destination (common under `.git`)
            // fails on Windows, so clear the bit first.
            if let Ok(existing) = fs::symlink_metadata(&destination) {
                let mut permissions = existing.permissions();
                if permissions.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    fs::set_permissions(&destination, permissions)?;
                }
            }
            copy_file(&source, &destination)?;
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

/// Rejects a write or removal whose destination chain contains a symlink:
/// `fs::copy` and `fs::remove_*` follow intermediate components, so a
/// directory replaced by a symlink inside the destination root would send
/// the operation outside the workspace. A missing ancestor is left to the
/// operation itself to report.
fn check_container_chain(root: &Path, destination: &Path) -> Result<()> {
    for directory in destination
        .ancestors()
        .skip(1)
        .take_while(|directory| *directory != root && !directory.as_os_str().is_empty())
    {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::Path(format!(
                    "refusing to operate through symlinked directory: {}",
                    directory.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Writes `destination` atomically: the copy lands in a sibling temp file
/// and is renamed over the target inside the same directory — atomic on
/// every filesystem, so a mid-write crash leaves a hidden `.rift.tmp`
/// leftover rather than a truncated file at the real path. The temp name
/// is namespaced to rift and unique per destination; a stale leftover is
/// safe to overwrite.
fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Path(format!("path has no parent: {}", destination.display())))?;
    let name = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let temporary = parent.join(format!(".rift.tmp.{}.{}", std::process::id(), name));
    let result = fs::copy(source, &temporary)
        .map_err(Error::from)
        .and_then(|_| {
            portable::copy_metadata(
                source,
                &temporary,
                portable::MetadataTarget::FileOrDirectory,
            )
        })
        .and_then(|_| fs::rename(&temporary, destination).map_err(Error::from));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
/// fingerprints are treated as unchanged: files compare by blake3 content
/// hash and mode, so a rewrite that preserves size and mtime still counts
/// as a change while a pure `touch` does not. Symlinks compare by target
/// and mode; directories by mode only, since a directory's own mtime shifts
/// whenever a child changes. Entries that are neither file, directory, nor
/// symlink (fifos, sockets, devices) are recorded as `Other`: they show up
/// in diffs but can never be applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) kind: EntryKind,
    pub(crate) mode: u32,
    pub(crate) link_target: Option<PathBuf>,
    pub(crate) hash: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

impl Entry {
    /// A path `theirs` can deliver through `apply_diff`.
    pub(crate) fn is_supported(&self) -> bool {
        !matches!(self.kind, EntryKind::Other)
    }
}

/// Path components that never participate in diffs or lands: git internals
/// (workspaces land git state through git, never file replay), rift's own
/// storage directories, which can sit inside a workspace that hosts
/// another workspace family, and `.rift.tmp*` debris — the marker's
/// in-flight write and `apply_diff`'s per-file copy temps, which a crash
/// can leave behind.
pub(crate) fn is_internal(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name)
                if name == ".git"
                    || name == ".rifts"
                    || name == ".trash"
                    || name == ".rifts-images"
                    || name.as_encoded_bytes().starts_with(b".rift.tmp")
        )
    })
}

/// Walks `root` and records a fingerprint per entry. The workspace's own
/// `.rift` marker and internal paths are skipped; `skip` excludes
/// additional relative paths (the base manifest's visibility rules).
pub(crate) fn manifest(
    root: &Path,
    skip: &dyn Fn(&Path) -> bool,
) -> Result<BTreeMap<PathBuf, Entry>> {
    let marker = marker::path(root);
    let marker_tmp = root.join(".rift.tmp");
    let mut entries = BTreeMap::new();
    for entry in WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .is_ok_and(|path| !is_internal(path) && !skip(path))
        })
    {
        let entry = entry?;
        let path = entry.path();
        // `.rift.tmp` is the in-flight marker write; a crash can leave it.
        if path == marker || path == marker_tmp {
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
            EntryKind::Other
        };
        entries.insert(
            relative.to_path_buf(),
            Entry {
                kind,
                mode: mode(&metadata),
                link_target: if file_type.is_symlink() {
                    Some(fs::read_link(path)?)
                } else {
                    None
                },
                hash: if file_type.is_file() {
                    Some(*hash_file(path)?.as_bytes())
                } else {
                    None
                },
            },
        );
    }
    Ok(entries)
}

fn hash_file(path: &Path) -> Result<blake3::Hash> {
    let mut file = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize())
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
