//! Three-way land and sync.
//!
//! A rift records a base manifest at creation: the exact contents of the
//! copy it started from. `land` and `sync` then merge three states:
//!
//! - `base`    — the recorded starting point,
//! - `ours`    — the workspace being written (the parent for `land`, the
//!   rift for `sync`),
//! - `theirs`  — the workspace being replayed.
//!
//! For every path: a side that still matches the base never causes a write;
//! a side that changed while the other stayed at the base is applied; when
//! both sides changed, identical results are a no-op and divergent results
//! are a conflict. Paths the copy filter excluded at creation are recorded
//! as `Excluded` and stay invisible to every diff, land, and sync, so
//! regenerable folders are never deleted or copied by a merge.
//!
//! After a merge, the base advances to the merged state: applied and
//! already-equal paths take `theirs`' entry, while conflicted paths keep
//! their old base entry, so an unresolved conflict is never reclassified
//! as clean by a later `land` or `sync`.

use crate::diff::{DiffEntry, DiffKind, Entry, EntryKind, TreeDiff, manifest};
use crate::filter::CopyFilter;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// What to do when a land or sync finds paths both sides changed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Apply the clean paths and report the conflicts.
    #[default]
    Report,
    /// Write nothing when any path conflicts.
    Abort,
    /// Take the incoming side's version for conflicting paths too.
    Force,
}

/// Options for `land` and `sync`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LandOptions {
    pub on_conflict: OnConflict,
    /// Merge only working-tree files. Required for workspaces inside Git
    /// repositories, where `.git` belongs to Git and never participates in
    /// a merge either way.
    pub files_only: bool,
}

impl LandOptions {
    pub fn on_conflict(mut self, on_conflict: OnConflict) -> Self {
        self.on_conflict = on_conflict;
        self
    }

    pub fn files_only(mut self, files_only: bool) -> Self {
        self.files_only = files_only;
        self
    }
}

/// A path both sides changed incompatibly. `ours`/`theirs` describe the
/// change each side made relative to the base.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConflictEntry {
    #[serde(serialize_with = "crate::diff::serialize_path")]
    pub path: PathBuf,
    pub ours: DiffKind,
    pub theirs: DiffKind,
}

/// The result of a three-way `land` or `sync`.
#[derive(Clone, Debug, Serialize)]
pub struct LandOutcome {
    /// The changes written into the receiving workspace.
    pub applied: TreeDiff,
    /// Paths both sides changed incompatibly. They were not applied unless
    /// the merge ran with `OnConflict::Force`.
    pub conflicts: Vec<ConflictEntry>,
}

/// The recorded state of one path in a rift's base manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BaseEntry {
    /// The copy filter left this path out of the rift; it is invisible to
    /// diffs and merges.
    Excluded,
    /// The path was copied with this fingerprint.
    Present(Entry),
}

/// The snapshot a rift started from, persisted per rift in the registry.
#[derive(Clone, Debug, Default)]
pub(crate) struct BaseManifest {
    /// Whether the rift was created with a filtered copy. Filtered rifts
    /// also treat every filter-matching path that is new since the base as
    /// invisible, so a rift-installed `node_modules` never lands into the
    /// parent.
    pub(crate) filtered: bool,
    pub(crate) entries: BTreeMap<PathBuf, BaseEntry>,
}

impl BaseManifest {
    /// An empty base for rifts recorded before base manifests existed:
    /// every divergent path reads as both-sides-changed, which fails safe
    /// (conflicts) rather than silently taking one side.
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Records the base for a rift just copied to `destination` from
    /// `source`. Filtered copies mark every path the filter skipped.
    pub(crate) fn record(source: &Path, destination: &Path, filtered: bool) -> Result<Self> {
        let mut entries = manifest(destination, &|_| false)?
            .into_iter()
            .map(|(path, entry)| (path, BaseEntry::Present(entry)))
            .collect::<BTreeMap<_, _>>();
        if filtered {
            for path in filtered_out(source)? {
                entries.insert(path, BaseEntry::Excluded);
            }
        }
        Ok(Self { filtered, entries })
    }

    /// A path the merge must not see: recorded exclusions plus, for
    /// filtered bases, anything the copy filter would leave out now.
    pub(crate) fn invisible(&self, path: &Path) -> bool {
        matches!(self.entries.get(path), Some(BaseEntry::Excluded))
            || (self.filtered && CopyFilter.excludes(path))
    }

    /// The changes a rift's current `entries` made relative to this base —
    /// what `land` would try to apply.
    pub(crate) fn diff(&self, current: &BTreeMap<PathBuf, Entry>) -> Vec<DiffEntry> {
        let mut paths = BTreeSet::new();
        paths.extend(current.keys().cloned());
        paths.extend(
            self.entries
                .keys()
                .filter(|path| !self.invisible(path))
                .cloned(),
        );
        let mut entries = Vec::new();
        for path in paths {
            let b = self.base_entry(&path);
            let r = current.get(&path);
            if r == b {
                continue;
            }
            entries.push(DiffEntry {
                path,
                kind: match (b, r) {
                    (None, Some(_)) => DiffKind::Added,
                    (Some(_), None) => DiffKind::Removed,
                    _ => DiffKind::Changed,
                },
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries
    }

    fn base_entry(&self, path: &Path) -> Option<&Entry> {
        match self.entries.get(path) {
            Some(BaseEntry::Present(entry)) => Some(entry),
            _ => None,
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![BASE_VERSION, u8::from(self.filtered)];
        write_u32(&mut out, self.entries.len() as u32);
        for (path, entry) in &self.entries {
            write_path(&mut out, path);
            match entry {
                BaseEntry::Excluded => out.push(0),
                BaseEntry::Present(entry) => {
                    out.push(1);
                    write_entry(&mut out, entry);
                }
            }
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        if reader.byte()? != BASE_VERSION {
            return Err(Error::CorruptBase("unknown base manifest version".into()));
        }
        let filtered = match reader.byte()? {
            0 => false,
            1 => true,
            tag => return Err(Error::CorruptBase(format!("unknown filtered flag {tag}"))),
        };
        let count = reader.u32()? as usize;
        let mut entries = BTreeMap::new();
        for _ in 0..count {
            let path = reader.path()?;
            let entry = match reader.byte()? {
                0 => BaseEntry::Excluded,
                1 => BaseEntry::Present(read_entry(&mut reader)?),
                tag => {
                    return Err(Error::CorruptBase(format!("unknown base entry tag {tag}")));
                }
            };
            if entries.insert(path, entry).is_some() {
                return Err(Error::CorruptBase("duplicate path in base manifest".into()));
            }
        }
        if reader.remaining() != 0 {
            return Err(Error::CorruptBase("trailing bytes in base manifest".into()));
        }
        Ok(Self { filtered, entries })
    }
}

/// The merge a `land` or `sync` would perform, before anything is written.
pub(crate) struct MergePlan {
    /// Paths `theirs` changed while `ours` stayed at the base.
    pub(crate) clean: Vec<DiffEntry>,
    /// Paths both sides changed incompatibly.
    pub(crate) conflicts: Vec<PlannedConflict>,
    /// Base entries after the clean paths land: conflicted paths keep
    /// their old values so the conflict stays visible to future merges.
    pub(crate) next_base: BTreeMap<PathBuf, BaseEntry>,
}

/// A conflict plus the entries needed to force-apply it.
pub(crate) struct PlannedConflict {
    pub(crate) entry: ConflictEntry,
    pub(crate) theirs: Option<Entry>,
}

impl PlannedConflict {
    /// The apply entry when this conflict is force-resolved.
    pub(crate) fn forced(&self) -> DiffEntry {
        DiffEntry {
            path: self.entry.path.clone(),
            kind: self.entry.theirs,
        }
    }
}

/// Computes the three-way merge of `theirs` into `ours` against `base`.
/// Nothing is written; callers replay `plan.clean` (and force-applied
/// conflicts) through `apply_diff`.
pub(crate) fn plan(base: &BaseManifest, ours_root: &Path, theirs_root: &Path) -> Result<MergePlan> {
    let skip = |path: &Path| base.invisible(path);
    let ours = manifest(ours_root, &skip)?;
    let theirs = manifest(theirs_root, &skip)?;

    let mut paths = BTreeSet::new();
    paths.extend(ours.keys().cloned());
    paths.extend(theirs.keys().cloned());
    paths.extend(
        base.entries
            .keys()
            .filter(|path| !base.invisible(path))
            .cloned(),
    );

    let mut clean = Vec::new();
    let mut conflicts = Vec::new();
    let mut next_base = base.entries.clone();
    // What `ours` will look like at each visited path once the clean
    // entries apply: `true` means a real directory that can hold writes.
    // `paths` visits ancestors before their descendants, so a write can
    // verify its whole container chain before it is classified clean —
    // without this, a clean add lands in a directory `ours` deleted,
    // kind-changed, or turned into a symlink (which would write through
    // the link, outside the workspace).
    let mut usable_dir: BTreeMap<PathBuf, bool> = BTreeMap::new();
    let mut conflicted: BTreeSet<PathBuf> = BTreeSet::new();
    // On a case-insensitive volume two byte-distinct names are one
    // directory slot: writing `Report.txt` over `report.txt` is a silent
    // overwrite, not an add. Probed once and folded maps built lazily, so
    // clean merges pay nothing for the check.
    let mut insensitive: Option<bool> = None;
    let mut folded_ours: Option<BTreeMap<Vec<u8>, PathBuf>> = None;
    let mut folded_theirs: Option<BTreeMap<Vec<u8>, Vec<PathBuf>>> = None;
    let mut name_collides = |path: &Path| {
        if !*insensitive.get_or_insert_with(|| volume_ignores_case(ours_root)) {
            return false;
        }
        let folded_ours = folded_ours.get_or_insert_with(|| fold_map(&ours));
        let folded_theirs = folded_theirs.get_or_insert_with(|| fold_map_multi(&theirs));
        let key = fold_key(path);
        folded_ours
            .get(&key)
            .is_some_and(|other| other.as_path() != path)
            || folded_theirs.get(&key).is_some_and(|twins| twins.len() > 1)
    };
    for path in paths {
        let b = base.base_entry(&path);
        let o = ours.get(&path);
        let t = theirs.get(&path);
        if t == b {
            // The incoming side did not change it: never write, and keep
            // the base so `ours`' own change stays visible to later merges.
            usable_dir.insert(
                path,
                o.is_some_and(|entry| entry.kind == EntryKind::Directory),
            );
            continue;
        }
        if o == b {
            // Only the incoming side changed it. Replacing a directory
            // whose subtree `ours` changed underneath is a conflict, not a
            // silent deletion of that work.
            let kind = match (o, t) {
                (None, Some(_)) => DiffKind::Added,
                (Some(_), Some(_)) => DiffKind::Changed,
                (Some(_), None) => DiffKind::Removed,
                (None, None) => continue,
            };
            let loses_subtree = b.is_some_and(|b| b.kind == EntryKind::Directory)
                && t.is_none_or(|entry| entry.kind != EntryKind::Directory);
            if loses_subtree && subtree_diverged(&path, &ours, base) {
                conflicts.push(planned(path.clone(), b, o, t));
                conflicted.insert(path.clone());
                usable_dir.insert(path, false);
                continue;
            }
            match t {
                Some(entry) if !entry.is_supported() => {
                    conflicts.push(planned(path.clone(), b, o, t));
                    conflicted.insert(path.clone());
                    usable_dir.insert(path, false);
                }
                _ if kind != DiffKind::Removed && !containers_usable(&path, &usable_dir) => {
                    // An ancestor is missing, not a directory, or itself
                    // conflicted in `ours` — a delete-vs-modify conflict,
                    // never a write.
                    escalate_blocked_ancestors(
                        &path,
                        base,
                        &ours,
                        &theirs,
                        &usable_dir,
                        &mut conflicted,
                        &mut conflicts,
                    );
                    conflicts.push(planned(path.clone(), b, o, t));
                    conflicted.insert(path.clone());
                    usable_dir.insert(path, false);
                }
                _ if kind != DiffKind::Removed && name_collides(&path) => {
                    // The write would land on a differently-cased sibling:
                    // one filesystem slot, two names — a conflict, not an
                    // overwrite.
                    conflicts.push(planned(path.clone(), b, o, t));
                    conflicted.insert(path.clone());
                    usable_dir.insert(path, false);
                }
                _ => {
                    clean.push(DiffEntry {
                        path: path.clone(),
                        kind,
                    });
                    usable_dir.insert(
                        path.clone(),
                        t.is_some_and(|entry| entry.kind == EntryKind::Directory),
                    );
                    set_base(&mut next_base, path, t);
                }
            }
            continue;
        }
        if o == t {
            // Both sides ended up identical: converged without a write.
            usable_dir.insert(
                path.clone(),
                o.is_some_and(|entry| entry.kind == EntryKind::Directory),
            );
            set_base(&mut next_base, path, t);
            continue;
        }
        // A conflicted path may still be force-applied, so a broken
        // ours-side container chain escalates exactly like a clean
        // write's: the incoming side's ancestor directories are restored
        // before the conflicted child is written.
        escalate_blocked_ancestors(
            &path,
            base,
            &ours,
            &theirs,
            &usable_dir,
            &mut conflicted,
            &mut conflicts,
        );
        conflicts.push(planned(path.clone(), b, o, t));
        conflicted.insert(path.clone());
        usable_dir.insert(path, false);
    }
    clean.sort_by(|a, b| a.path.cmp(&b.path));
    conflicts.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
    Ok(MergePlan {
        clean,
        conflicts,
        next_base,
    })
}

/// Marks `path`'s ancestors that cannot hold a write in post-merge `ours`
/// — missing, not a directory, or themselves conflicted — as conflicts,
/// so a forced merge restores the incoming side's container chain before
/// it writes the child.
fn escalate_blocked_ancestors(
    path: &Path,
    base: &BaseManifest,
    ours: &BTreeMap<PathBuf, Entry>,
    theirs: &BTreeMap<PathBuf, Entry>,
    usable_dir: &BTreeMap<PathBuf, bool>,
    conflicted: &mut BTreeSet<PathBuf>,
    conflicts: &mut Vec<PlannedConflict>,
) {
    for ancestor in proper_ancestors(path) {
        if usable_dir.get(ancestor) == Some(&false) && !conflicted.contains(ancestor) {
            conflicts.push(planned(
                ancestor.to_path_buf(),
                base.base_entry(ancestor),
                ours.get(ancestor),
                theirs.get(ancestor),
            ));
            conflicted.insert(ancestor.to_path_buf());
        }
    }
}

/// The proper ancestors of `path` inside the workspace, nearest first.
fn proper_ancestors(path: &Path) -> impl Iterator<Item = &Path> {
    path.ancestors()
        .skip(1)
        .take_while(|ancestor| !ancestor.as_os_str().is_empty())
}

/// Every ancestor of `path` must resolve to a real directory in `ours`
/// after the merge: directories created by this plan count, while a
/// missing, non-directory, or conflicted ancestor means the write could
/// fail mid-apply — or escape the workspace through a symlink.
fn containers_usable(path: &Path, usable_dir: &BTreeMap<PathBuf, bool>) -> bool {
    proper_ancestors(path).all(|ancestor| usable_dir.get(ancestor).copied().unwrap_or(false))
}

/// The name a case-insensitive filesystem would store: Unicode-lowercased
/// when the path is UTF-8, ASCII-lowercased byte-wise otherwise so two
/// distinct names can never collapse into a false match.
fn fold_key(path: &Path) -> Vec<u8> {
    match path.to_str() {
        Some(text) => text.to_lowercase().into_bytes(),
        None => path.as_os_str().as_encoded_bytes().to_ascii_lowercase(),
    }
}

fn fold_map(manifest: &BTreeMap<PathBuf, Entry>) -> BTreeMap<Vec<u8>, PathBuf> {
    manifest
        .keys()
        .map(|path| (fold_key(path), path.clone()))
        .collect()
}

fn fold_map_multi(manifest: &BTreeMap<PathBuf, Entry>) -> BTreeMap<Vec<u8>, Vec<PathBuf>> {
    let mut map: BTreeMap<Vec<u8>, Vec<PathBuf>> = BTreeMap::new();
    for path in manifest.keys() {
        map.entry(fold_key(path)).or_default().push(path.clone());
    }
    map
}

/// Whether the volume holding `root` ignores letter case: `.RIFT` only
/// resolves to the `.rift` marker when the filesystem folds the name, and
/// comparing file identity keeps a real `.RIFT` file on a case-sensitive
/// volume from looking folded.
fn volume_ignores_case(root: &Path) -> bool {
    same_file(&root.join(".rift"), &root.join(".RIFT"))
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(windows)]
fn same_file(a: &Path, b: &Path) -> bool {
    let (Some(a), Some(b)) = (
        crate::strategy::portable::by_handle_info(a),
        crate::strategy::portable::by_handle_info(b),
    ) else {
        return false;
    };
    a.dwVolumeSerialNumber == b.dwVolumeSerialNumber
        && a.nFileIndexHigh == b.nFileIndexHigh
        && a.nFileIndexLow == b.nFileIndexLow
}

#[cfg(not(any(unix, windows)))]
fn same_file(_a: &Path, _b: &Path) -> bool {
    false
}

/// What one side did to `path` relative to the base.
fn side_change(b: Option<&Entry>, side: Option<&Entry>) -> DiffKind {
    match (b, side) {
        (None, Some(_)) => DiffKind::Added,
        (Some(_), None) => DiffKind::Removed,
        _ => DiffKind::Changed,
    }
}

fn planned(
    path: PathBuf,
    b: Option<&Entry>,
    o: Option<&Entry>,
    t: Option<&Entry>,
) -> PlannedConflict {
    PlannedConflict {
        entry: ConflictEntry {
            path,
            ours: side_change(b, o),
            theirs: side_change(b, t),
        },
        theirs: t.cloned(),
    }
}

/// Advances the base over a force-resolved conflict: the incoming side's
/// entry becomes the merged state.
pub(crate) fn set_resolved(
    next_base: &mut BTreeMap<PathBuf, BaseEntry>,
    conflict: &PlannedConflict,
) {
    set_base(
        next_base,
        conflict.entry.path.clone(),
        conflict.theirs.as_ref(),
    );
}

/// Records the merged state of a resolved path: the incoming side's entry,
/// or nothing when that side deleted it.
fn set_base(next_base: &mut BTreeMap<PathBuf, BaseEntry>, path: PathBuf, theirs: Option<&Entry>) {
    match theirs {
        Some(entry) => next_base.insert(path, BaseEntry::Present(entry.clone())),
        None => next_base.remove(&path),
    };
}

/// `ours` changed something under directory `path` since the base when a
/// descendant exists that the base does not know or that has different
/// content. `ours`-deleted descendants do not count: removing the directory
/// removes them either way.
fn subtree_diverged(path: &Path, ours: &BTreeMap<PathBuf, Entry>, base: &BaseManifest) -> bool {
    // `path` itself compares equal to its base entry here (the caller only
    // runs this when `ours == base` at `path`), so starting the range at
    // `path` is harmless and catches every descendant regardless of name.
    ours.range(path.to_path_buf()..)
        .take_while(|(descendant, _)| descendant.starts_with(path))
        .any(|(descendant, entry)| base.base_entry(descendant) != Some(entry))
}

const BASE_VERSION: u8 = 1;

fn kind_tag(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
        EntryKind::Symlink => 2,
        EntryKind::Other => 3,
    }
}

fn tag_kind(tag: u8) -> Result<EntryKind> {
    match tag {
        0 => Ok(EntryKind::Directory),
        1 => Ok(EntryKind::File),
        2 => Ok(EntryKind::Symlink),
        3 => Ok(EntryKind::Other),
        tag => Err(Error::CorruptBase(format!("unknown entry kind {tag}"))),
    }
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_path(out: &mut Vec<u8>, path: &Path) {
    let bytes = path.as_os_str().as_encoded_bytes();
    write_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn write_entry(out: &mut Vec<u8>, entry: &Entry) {
    out.push(kind_tag(entry.kind));
    write_u32(out, entry.mode);
    match &entry.link_target {
        Some(target) => {
            out.push(1);
            write_path(out, target);
        }
        None => out.push(0),
    }
    match &entry.hash {
        Some(hash) => {
            out.push(1);
            out.extend_from_slice(hash);
        }
        None => out.push(0),
    }
}

/// Paths under `root` that the copy filter would leave out of a rift, used
/// to mark the base manifest's `Excluded` entries.
fn filtered_out(root: &Path) -> Result<Vec<PathBuf>> {
    let mut excluded = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let Ok(relative) = entry.path().strip_prefix(root) else {
                return true;
            };
            if crate::diff::is_internal(relative) {
                return false;
            }
            if CopyFilter.excludes(relative) {
                excluded.push(relative.to_path_buf());
                return false;
            }
            true
        })
    {
        entry?;
    }
    Ok(excluded)
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        if self.remaining() < count {
            return Err(Error::CorruptBase("truncated base manifest".into()));
        }
        let slice = &self.bytes[self.offset..self.offset + count];
        self.offset += count;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn path(&mut self) -> Result<PathBuf> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        Ok(PathBuf::from(os_str_from_bytes(bytes)?))
    }
}

#[cfg(unix)]
fn os_str_from_bytes(bytes: &[u8]) -> Result<&OsStr> {
    use std::os::unix::ffi::OsStrExt;
    Ok(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn os_str_from_bytes(bytes: &[u8]) -> Result<&OsStr> {
    // There is no safe WTF-8 validator on this platform: a blob path that
    // is not UTF-8 is reported as corrupt rather than decoded unchecked.
    std::str::from_utf8(bytes)
        .map(OsStr::new)
        .map_err(|_| Error::CorruptBase("non-UTF-8 path in base manifest".into()))
}

fn read_entry(reader: &mut Reader<'_>) -> Result<Entry> {
    let kind = tag_kind(reader.byte()?)?;
    let mode = reader.u32()?;
    let link_target = match reader.byte()? {
        0 => None,
        1 => Some(reader.path()?),
        tag => return Err(Error::CorruptBase(format!("unknown link flag {tag}"))),
    };
    let hash = match reader.byte()? {
        0 => None,
        1 => Some(reader.take(32)?.try_into().unwrap()),
        tag => return Err(Error::CorruptBase(format!("unknown hash flag {tag}"))),
    };
    Ok(Entry {
        kind,
        mode,
        link_target,
        hash,
    })
}
