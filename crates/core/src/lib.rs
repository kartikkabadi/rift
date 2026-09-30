mod config;
pub mod cow_image;
mod diff;
mod filter;
mod git;
mod hook;
mod id;
mod marker;
mod merge;
mod name;
mod registry;
pub mod rpc;
mod strategy;

#[cfg(all(test, target_os = "linux"))]
mod linux_filesystem_tests;
#[cfg(all(test, target_os = "linux"))]
mod test_support;

use id::RiftId;
use name::RiftName;
use registry::{MovedRecord, PathRecord, Record, Registry, SubtreeScope};
use std::fs;
use std::path::{Path, PathBuf};
use strategy::{Strategy, StrategyInit};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{operation} failed for {path}: {source}")]
    IoAt {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Database(#[from] rusqlite::Error),
    #[error("{0}")]
    Walk(#[from] walkdir::Error),
    #[error("invalid path: {0}")]
    Path(String),
    #[error("copy-on-write cloning unavailable: {0}")]
    CowUnavailable(String),
    #[error("workspace requires initialization: {0}")]
    InitializationRequired(PathBuf),
    #[error("workspace is not initialized: {0}")]
    WorkspaceNotInitialized(PathBuf),
    #[error("rift marker is missing: {0}")]
    MissingMarker(PathBuf),
    #[error("unsupported filesystem entry: {0}")]
    UnsupportedEntry(PathBuf),
    #[error("unsafe Git source: {0}")]
    UnsafeGit(String),
    #[error("directory is not managed by rift: {0}")]
    NotManaged(PathBuf),
    #[error("rift marker does not match the registry at: {0}")]
    MarkerMismatch(PathBuf),
    #[error("rift marker belongs to an unknown registry entry at: {0}")]
    UnknownMarker(PathBuf),
    #[error("rift directory already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("every generated rift name is already in use under: {0}")]
    NamesExhausted(PathBuf),
    #[error("cannot remove subtree while a recorded rift path is missing: {0}")]
    MissingRift(PathBuf),
    #[error("workspace path overlaps another managed workspace: {0}")]
    OverlappingWorkspace(PathBuf),
    #[error("invalid rift config at {path}: {message}")]
    InvalidConfig { path: PathBuf, message: String },
    #[error("{hook} hook failed at {path}: `{command}` {message}")]
    HookFailed {
        hook: String,
        path: PathBuf,
        command: String,
        message: String,
    },
    #[error("copy-on-write image setup failed: {0}")]
    CowImageSetup(String),
    #[error("workspace has no parent to {operation} with: {path}")]
    NoParent {
        path: PathBuf,
        operation: &'static str,
    },
    /// `land`/`sync` reached a workspace inside a Git repository without
    /// `files_only`: repository state belongs to Git and is never replayed
    /// file-by-file.
    #[error(
        "{0} is a Git repository; land or sync it through Git, or pass filesOnly to merge working-tree files only"
    )]
    UseGit(PathBuf),
    /// `land`/`sync` ran with `on_conflict: abort` and found conflicting
    /// paths; nothing was written.
    #[error(
        "{operation} found {conflicts} conflicting path(s) in {path}; rerun with onConflict 'report' to apply clean paths or 'force' to take the incoming side"
    )]
    LandConflict {
        operation: &'static str,
        path: PathBuf,
        conflicts: usize,
    },
    /// Another `create`, `land`, `sync`, or `remove` holds the root
    /// workspace's lock.
    #[error("{0} is locked by another rift operation")]
    Locked(PathBuf),
    /// A recorded base manifest cannot be decoded; the rift's merge base is
    /// unknown, so landing it cannot be proven safe.
    #[error("recorded base manifest is corrupt: {0}")]
    CorruptBase(String),
}

pub use diff::{DiffEntry, DiffKind, TreeDiff};
pub use merge::{ConflictEntry, LandOptions, LandOutcome, OnConflict};

pub struct Create {
    pub from: PathBuf,
    pub name: Option<String>,
    pub into: Option<PathBuf>,
}

impl Create {
    pub fn new(from: impl Into<PathBuf>) -> Self {
        Self {
            from: from.into(),
            name: None,
            into: None,
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
    }

    pub fn with_storage(mut self, into: Option<PathBuf>) -> Self {
        self.into = into;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CreateOptions {
    pub copy_mode: CopyMode,
    pub hook_mode: HookMode,
    pub cow_mode: CowMode,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RemoveOptions {
    pub hook_mode: HookMode,
}

impl RemoveOptions {
    pub fn hook_mode(mut self, hook_mode: HookMode) -> Self {
        self.hook_mode = hook_mode;
        self
    }
}

impl CreateOptions {
    pub fn copy_mode(mut self, copy_mode: CopyMode) -> Self {
        self.copy_mode = copy_mode;
        self
    }

    pub fn cow_mode(mut self, cow_mode: CowMode) -> Self {
        self.cow_mode = cow_mode;
        self
    }

    pub fn hook_mode(mut self, hook_mode: HookMode) -> Self {
        self.hook_mode = hook_mode;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyMode {
    Filtered,
    All,
}

impl Default for CopyMode {
    fn default() -> Self {
        Self::Filtered
    }
}

/// Whether workspace creation may fall back to a regular full copy when the
/// filesystem offers no copy-on-write mechanism.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CowMode {
    /// Use copy-on-write when available, otherwise a regular copy.
    #[default]
    Auto,
    /// Fail with `Error::CowUnavailable` instead of falling back.
    Require,
}

/// The copy mechanism `create` will use for a workspace path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Writable btrfs subvolume snapshots.
    Btrfs,
    /// Native per-file reflinks on non-btrfs Linux filesystems.
    Reflink,
    /// APFS `clonefile`.
    Apfs,
    /// ReFS block cloning on Windows.
    #[serde(rename = "refs")]
    ReFs,
    /// A regular file-by-file copy; works on every filesystem.
    Portable,
}

/// The result of probing what a path supports.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Probe {
    #[serde(serialize_with = "crate::diff::serialize_path")]
    pub path: PathBuf,
    pub backend: Backend,
    /// The filesystem type name when the platform can report it.
    pub filesystem: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookMode {
    Run,
    Skip,
}

impl Default for HookMode {
    fn default() -> Self {
        Self::Run
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitProgress {
    CreatingSubvolume,
    ImportingWorkspace,
    ImportedEntries { entries: u64 },
    ActivatingWorkspace,
    RemovingOriginal,
    RestoringMarker,
    RegisteringWorkspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InitOutcome {
    Registered,
    AlreadyInitialized,
    Converted,
    /// Registered, but the filesystem offers no copy-on-write mechanism, so
    /// new workspaces will be regular copies unless `CowMode::Require` is set.
    Degraded,
}

impl InitOutcome {
    pub fn is_converted(self) -> bool {
        matches!(self, Self::Converted)
    }

    pub fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded)
    }
}

pub struct Manager {
    registry: Registry,
    strategy: Box<dyn Strategy>,
}

impl Manager {
    pub fn open_default() -> Result<Self> {
        let path = default_database_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::open(path)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_strategy(path, strategy::default_strategy())
    }

    fn with_strategy(path: impl AsRef<Path>, strategy: Box<dyn Strategy>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let registry = Registry::open(path)?;
        Ok(Self { registry, strategy })
    }

    pub fn create(&mut self, input: Create) -> Result<PathBuf> {
        self.create_with_options(input, CreateOptions::default())
    }

    pub fn create_with_options(
        &mut self,
        input: Create,
        options: CreateOptions,
    ) -> Result<PathBuf> {
        let requested = existing_directory(&input.from)?;
        let source = self.workspace_from(&requested)?;
        let git = git::check_source(&source.path)?;
        let root = self.root(&source)?;
        // The copy must not interleave with a `land`/`sync`/`remove`
        // writing the same workspace family.
        self.with_root_lock(&root, |this| {
            this.create_locked(&source, &root, input, options, git)
        })
    }

    /// The body of `create_with_options` once the family lock is held.
    fn create_locked(
        &mut self,
        source: &Record,
        root: &Record,
        input: Create,
        options: CreateOptions,
        git: git::Source,
    ) -> Result<PathBuf> {
        let from = source.path.clone();
        let id = RiftId::new();
        let destination_parent = match input.into {
            Some(path) => absolute_path(&path)?,
            None => default_storage(&root.path)?,
        };
        let managed_paths = self.managed_paths()?;
        if managed_paths
            .iter()
            .any(|record| destination_parent.starts_with(&record.path))
        {
            return Err(Error::OverlappingWorkspace(destination_parent));
        }
        fs::create_dir_all(&destination_parent)?;
        let destination_parent = fs::canonicalize(destination_parent)?;
        let name = match input.name {
            Some(name) => RiftName::new(name)?,
            None => name::generated()
                .find(|name| !destination_parent.join(name.as_str()).exists())
                .ok_or_else(|| Error::NamesExhausted(destination_parent.clone()))?,
        };
        let destination = destination_parent.join(name.as_str());
        if destination.exists() {
            return Err(Error::AlreadyExists(destination));
        }
        if managed_paths
            .iter()
            .any(|record| paths_overlap(&destination, &record.path))
        {
            return Err(Error::OverlappingWorkspace(destination));
        }
        let config = match options.hook_mode {
            HookMode::Run => config::Config::load(&from)?,
            HookMode::Skip => config::Config::default(),
        };

        hook::run(
            "precreate",
            config.precreate(),
            &from,
            &from,
            &destination,
            &id,
            &source.id,
        )?;

        if let Err(error) =
            self.strategy
                .copy_directory(&from, &destination, options.copy_mode, options.cow_mode)
        {
            if !matches!(&error, Error::AlreadyExists(path) if path == &destination)
                && destination.exists()
            {
                let _ = self.strategy.remove_directory(&destination);
            }
            return Err(error);
        }

        let result: Result<()> = (|| {
            marker::write(&destination, &id)?;
            if git.is_repository() {
                #[cfg(windows)]
                git::make_writable(&destination)?;
                git::hide_marker(&destination)?;
                git::detach_destination(&destination)?;
            }
            if git.is_repository() {
                git::hide_marker(&from)?;
            }
            // The base is recorded in the same transaction as the rift row,
            // so a rift can never exist without the manifest its `land` and
            // `sync` merge against.
            let base = merge::BaseManifest::record(
                &from,
                &destination,
                options.copy_mode == CopyMode::Filtered,
            )?;
            self.registry.insert_child_with_base(
                &id,
                &source.id,
                &destination,
                &base.encode(),
                git::head_commit(&destination).as_deref(),
            )?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.strategy.remove_directory(&destination);
        }
        result?;
        hook::run(
            "postcreate",
            config.postcreate(),
            &destination,
            &from,
            &destination,
            &id,
            &source.id,
        )?;
        Ok(destination)
    }

    pub fn init(&mut self, at: impl AsRef<Path>) -> Result<InitOutcome> {
        self.init_with_cow_mode(at, CowMode::Auto, |_| {})
    }

    pub fn init_with_progress(
        &mut self,
        at: impl AsRef<Path>,
        progress: impl FnMut(InitProgress),
    ) -> Result<InitOutcome> {
        self.init_with_cow_mode(at, CowMode::Auto, progress)
    }

    pub fn init_with_cow_mode(
        &mut self,
        at: impl AsRef<Path>,
        cow_mode: CowMode,
        mut progress: impl FnMut(InitProgress),
    ) -> Result<InitOutcome> {
        let at = existing_directory(at.as_ref())?;
        let git = git::check_source(&at)?;
        if let Some(record) = self.registry.record_at(&at)? {
            if marker::read(&at)?.is_none() {
                progress(InitProgress::RestoringMarker);
                marker::write(&at, &record.id)?;
            } else {
                marker::verify(&record.path, &record.id)?;
            }
            let converted = self
                .strategy
                .initialize_directory(&at, &mut progress, cow_mode)?;
            if git.is_repository() {
                git::hide_marker(&at)?;
            }
            return Ok(match converted {
                StrategyInit::AlreadyNative => InitOutcome::AlreadyInitialized,
                StrategyInit::Converted => InitOutcome::Converted,
                StrategyInit::Degraded => InitOutcome::Degraded,
            });
        }
        if marker::read(&at)?.is_some() {
            return Err(Error::MarkerMismatch(at));
        }
        if self
            .managed_paths()?
            .iter()
            .any(|record| paths_overlap(&at, &record.path))
        {
            return Err(Error::OverlappingWorkspace(at));
        }

        let converted = self
            .strategy
            .initialize_directory(&at, &mut progress, cow_mode)?;
        progress(InitProgress::RegisteringWorkspace);
        let id = RiftId::new();
        let result = (|| {
            marker::write(&at, &id)?;
            if git.is_repository() {
                git::hide_marker(&at)?;
            }
            self.registry.insert_root(&id, &at)?;
            Ok(match converted {
                StrategyInit::AlreadyNative => InitOutcome::Registered,
                StrategyInit::Converted => InitOutcome::Converted,
                StrategyInit::Degraded => InitOutcome::Degraded,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(marker::path(&at));
        }
        result
    }

    pub fn remove(&mut self, at: impl AsRef<Path>) -> Result<()> {
        self.remove_with_options(at, RemoveOptions::default())
    }

    pub fn remove_with_options(
        &mut self,
        at: impl AsRef<Path>,
        options: RemoveOptions,
    ) -> Result<()> {
        let record = self.workspace_at(at)?;
        marker::verify(&record.path, &record.id)?;
        let config = self.remove_config(&record.path, options)?;
        let root = self.root(&record)?;
        self.with_root_lock(&root, |this| this.remove_locked(&record, config))
    }

    /// The body of `remove_with_options` once the family lock is held.
    fn remove_locked(&mut self, record: &Record, config: config::Config) -> Result<()> {
        let parent_id = record.parent_id.as_ref().unwrap_or(&record.id);
        hook::run(
            "preremove",
            config.preremove(),
            &record.path,
            &record.path,
            &record.path,
            &record.id,
            parent_id,
        )?;
        if record.parent_id.is_none() {
            self.unregister_root(record)?;
            return hook::run(
                "postremove",
                config.postremove(),
                &record.path,
                &record.path,
                &record.path,
                &record.id,
                parent_id,
            );
        }
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::IncludingRoot)?;
        self.trash_rows(&rows)?;
        let trashed = trash_path(&record.id, &record.path)?;
        hook::run(
            "postremove",
            config.postremove(),
            &trashed,
            &record.path,
            &trashed,
            &record.id,
            parent_id,
        )
    }

    pub fn remove_all(&mut self, at: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        self.remove_all_with_options(at, RemoveOptions::default())
    }

    pub fn remove_all_with_options(
        &mut self,
        at: impl AsRef<Path>,
        options: RemoveOptions,
    ) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(at)?;
        marker::verify(&record.path, &record.id)?;
        let config = self.remove_config(&record.path, options)?;
        let root = self.root(&record)?;
        self.with_root_lock(&root, |this| this.remove_all_locked(&record, config))
    }

    /// The body of `remove_all_with_options` once the family lock is held.
    fn remove_all_locked(
        &mut self,
        record: &Record,
        config: config::Config,
    ) -> Result<Vec<PathBuf>> {
        let parent_id = record.parent_id.as_ref().unwrap_or(&record.id);
        hook::run(
            "preremove",
            config.preremove(),
            &record.path,
            &record.path,
            &record.path,
            &record.id,
            parent_id,
        )?;
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::DescendantsOnly)?;
        self.trash_rows(&rows)?;
        hook::run(
            "postremove",
            config.postremove(),
            &record.path,
            &record.path,
            &record.path,
            &record.id,
            parent_id,
        )?;
        Ok(rows.into_iter().map(|record| record.path).collect())
    }

    fn remove_config(&self, workspace: &Path, options: RemoveOptions) -> Result<config::Config> {
        match options.hook_mode {
            HookMode::Run => config::Config::load(workspace),
            HookMode::Skip => Ok(config::Config::default()),
        }
    }

    fn unregister_root(&mut self, record: &Record) -> Result<()> {
        marker::verify(&record.path, &record.id)?;
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::DescendantsOnly)?;
        let existing = rows
            .into_iter()
            .filter(|record| record.path.exists())
            .collect::<Vec<_>>();
        self.trash_rows(&existing)?;
        fs::remove_file(marker::path(&record.path))?;
        let result = self.registry.delete_active(&record.id);
        if result.is_err() {
            let _ = marker::write(&record.path, &record.id);
        }
        result
    }

    fn trash_rows(&mut self, rows: &[PathRecord]) -> Result<()> {
        rows.iter().try_for_each(|row| -> Result<()> {
            row.path
                .exists()
                .then_some(())
                .ok_or_else(|| Error::MissingRift(row.path.clone()))?;
            marker::verify(&row.path, &row.id)?;
            Ok(())
        })?;
        let targets = rows
            .iter()
            .map(|row| {
                Ok(MovedRecord {
                    id: row.id.clone(),
                    original_path: row.path.clone(),
                    trash_path: trash_path(&row.id, &row.path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let managed_paths = self.managed_paths()?;
        for target in &targets {
            if managed_paths.iter().any(|record| {
                record.path != target.original_path
                    && paths_overlap(&target.original_path, &record.path)
            }) {
                return Err(Error::OverlappingWorkspace(target.original_path.clone()));
            }
        }
        targets.iter().try_for_each(|target| {
            (!target.trash_path.exists())
                .then_some(())
                .ok_or_else(|| Error::AlreadyExists(target.trash_path.clone()))
        })?;
        let mut moved: Vec<MovedRecord> = Vec::with_capacity(rows.len());
        for target in targets {
            let trash_parent = target.trash_path.parent().ok_or_else(|| {
                Error::Path(format!(
                    "trash path has no parent: {}",
                    target.trash_path.display()
                ))
            })?;
            fs::create_dir_all(trash_parent)?;
            if let Err(error) = fs::rename(&target.original_path, &target.trash_path) {
                for record in moved.iter().rev() {
                    let _ = fs::rename(&record.trash_path, &record.original_path);
                }
                return Err(error.into());
            }
            moved.push(target);
        }
        let result = self.registry.trash_moved(&moved);
        if result.is_err() {
            for record in moved.iter().rev() {
                let _ = fs::rename(&record.trash_path, &record.original_path);
            }
        }
        result
    }

    fn managed_paths(&self) -> Result<Vec<PathRecord>> {
        let mut paths = self.registry.active_paths()?;
        paths.extend(self.registry.trashed_paths()?);
        Ok(paths)
    }

    pub fn list(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(of)?;
        self.registry.child_paths(&record.id)
    }

    pub fn descendants(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(of)?;
        Ok(self
            .registry
            .subtree(&record.id, SubtreeScope::DescendantsOnly)?
            .into_iter()
            .map(|record| record.path)
            .collect())
    }

    pub fn ancestors(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(of)?;
        let mut paths = Vec::new();
        let mut parent_id = record.parent_id;
        while let Some(id) = parent_id {
            let parent = self
                .registry
                .record_id(&id)?
                .ok_or_else(|| Error::NotManaged(record.path.clone()))?;
            paths.push(parent.path);
            parent_id = parent.parent_id;
        }
        Ok(paths)
    }

    pub fn gc(&mut self) -> Result<Vec<PathBuf>> {
        let removed = self
            .registry
            .trashed_paths()?
            .into_iter()
            .map(|row| -> Result<PathBuf> {
                if row.path.exists() {
                    self.strategy.remove_directory(&row.path)?;
                }
                self.registry.delete_trash(&row.id)?;
                Ok(row.path)
            })
            .collect::<Result<Vec<_>>>()?;

        let missing = self
            .registry
            .active_paths()?
            .into_iter()
            .filter(|row| !row.path.exists())
            .map(|row| {
                self.registry
                    .subtree(&row.id, SubtreeScope::DescendantsOnly)
                    .map(|descendants| {
                        (!descendants
                            .iter()
                            .any(|descendant| descendant.path.exists()))
                        .then_some(row)
                    })
            })
            .filter_map(Result::transpose)
            .collect::<Result<Vec<_>>>()?;
        self.registry.delete_active_records(&missing)?;

        // A crash between the copy and the registry insert leaves an
        // unregistered folder under the storage root; sweep it.
        let mut swept = Vec::new();
        for root in self.registry.root_paths()? {
            // A held lock means another process is copying into this
            // family right now; leave its half-written tree for the next
            // pass.
            if !self.registry.lock_root(&root.id)? {
                continue;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<Vec<PathBuf>> {
                    let storage = match default_storage(&root.path) {
                        Ok(storage) => storage,
                        Err(_) => return Ok(Vec::new()),
                    };
                    if !storage.is_dir() {
                        return Ok(Vec::new());
                    }
                    let mut swept = Vec::new();
                    for entry in fs::read_dir(&storage)? {
                        let entry = entry?;
                        let candidate = entry.path();
                        let is_directory = entry
                            .file_type()
                            .map(|file_type| file_type.is_dir())
                            .unwrap_or(false);
                        // Internal folders (`.trash`, nested `.rifts`) belong
                        // to rift itself, not to any registry row.
                        if !is_directory || crate::diff::is_internal(Path::new(&entry.file_name()))
                        {
                            continue;
                        }
                        // Checked inside the lock, so a `create` that just
                        // committed is never swept.
                        if self.registry.record_at(&candidate)?.is_some() {
                            continue;
                        }
                        self.strategy.remove_directory(&candidate)?;
                        swept.push(candidate);
                    }
                    Ok(swept)
                },
            ));
            self.release_root(&root.id);
            match result {
                Ok(result) => swept.extend(result?),
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
        Ok(removed
            .into_iter()
            .chain(missing.into_iter().map(|record| record.path))
            .chain(swept)
            .collect())
    }

    pub fn workspace(&self, at: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(self.workspace_at(at)?.path)
    }

    /// The file-level changes inside the workspace at `at` relative to the
    /// base it was copied from — exactly what `land` would apply. Paths the
    /// copy filter excluded at creation are invisible. Rifts recorded before
    /// base manifests existed fall back to a raw comparison with the parent.
    pub fn diff(&self, at: impl AsRef<Path>) -> Result<TreeDiff> {
        let record = self.workspace_at(at)?;
        marker::verify(&record.path, &record.id)?;
        let parent = self.parent(&record, "compare")?;
        match self.base_manifest(&record.id)? {
            Some(base) => {
                let current = diff::manifest(&record.path, &|path| base.invisible(path))?;
                Ok(TreeDiff {
                    from: parent.path.clone(),
                    to: record.path.clone(),
                    entries: base.diff(&current),
                })
            }
            None => diff::diff_trees(&parent.path, &record.path, &|_| false),
        }
    }

    /// Applies the rift's changes into its parent workspace through a
    /// three-way merge against the rift's recorded base. Changes the parent
    /// made after the copy are preserved; paths both sides changed are
    /// reported as conflicts. `.git` is never touched, and the whole
    /// operation refuses a Git repository unless `files_only` is set.
    pub fn land(&mut self, at: impl AsRef<Path>) -> Result<LandOutcome> {
        self.land_with_options(at, LandOptions::default())
    }

    pub fn land_with_options(
        &mut self,
        at: impl AsRef<Path>,
        options: LandOptions,
    ) -> Result<LandOutcome> {
        let record = self.workspace_at(at)?;
        marker::verify(&record.path, &record.id)?;
        let parent = self.parent(&record, "land")?;
        marker::verify(&parent.path, &parent.id)?;
        let root = self.root(&record)?;
        self.with_root_lock(&root, |this| {
            this.merge_workspaces("land", &record, &parent.path, &record.path, options)
        })
    }

    /// The reverse of `land`: merges the parent's changes into the rift
    /// against the recorded base. Paths the rift changed are never
    /// overwritten — they surface as conflicts instead.
    pub fn sync(&mut self, at: impl AsRef<Path>) -> Result<LandOutcome> {
        self.sync_with_options(at, LandOptions::default())
    }

    pub fn sync_with_options(
        &mut self,
        at: impl AsRef<Path>,
        options: LandOptions,
    ) -> Result<LandOutcome> {
        let record = self.workspace_at(at)?;
        marker::verify(&record.path, &record.id)?;
        let parent = self.parent(&record, "sync")?;
        marker::verify(&parent.path, &parent.id)?;
        let root = self.root(&record)?;
        self.with_root_lock(&root, |this| {
            this.merge_workspaces("sync", &record, &record.path, &parent.path, options)
        })
    }

    /// Runs the three-way merge writing `ours` (the parent for `land`, the
    /// rift for `sync`) from `theirs`, then advances the rift's base.
    fn merge_workspaces(
        &mut self,
        operation: &'static str,
        record: &Record,
        ours: &Path,
        theirs: &Path,
        options: LandOptions,
    ) -> Result<LandOutcome> {
        let ours_git = git::check_source(ours)?;
        let theirs_git = git::check_source(theirs)?;
        if !options.files_only && (ours_git.is_repository() || theirs_git.is_repository()) {
            let repository = if ours_git.is_repository() {
                ours
            } else {
                theirs
            };
            return Err(Error::UseGit(repository.to_path_buf()));
        }

        let base = self
            .base_manifest(&record.id)?
            .unwrap_or_else(merge::BaseManifest::empty);
        let plan = merge::plan(&base, ours, theirs)?;
        if options.on_conflict == OnConflict::Abort && !plan.conflicts.is_empty() {
            return Err(Error::LandConflict {
                operation,
                path: ours.to_path_buf(),
                conflicts: plan.conflicts.len(),
            });
        }

        let mut entries = plan.clean;
        let mut conflicts = Vec::new();
        let mut next_base = merge::BaseManifest {
            filtered: base.filtered,
            entries: plan.next_base,
        };
        for conflict in plan.conflicts {
            if options.on_conflict == OnConflict::Force {
                let forced = conflict.forced();
                // On a folded volume a fold-twin write already absorbed
                // the shared filesystem slot (the incoming half of a
                // case-only rename); replaying the removal afterwards
                // would delete the entry just written.
                if forced.kind != DiffKind::Removed
                    || !plan.absorbed_removals.contains(&forced.path)
                {
                    entries.push(forced);
                }
                merge::set_resolved(&mut next_base.entries, &conflict);
            } else {
                conflicts.push(conflict.entry);
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let applied = TreeDiff {
            from: ours.to_path_buf(),
            to: theirs.to_path_buf(),
            entries,
        };
        diff::apply_diff(&applied)?;
        self.registry.update_base(&record.id, &next_base.encode())?;
        Ok(LandOutcome { applied, conflicts })
    }

    /// The recorded base manifest for a rift, decoded.
    fn base_manifest(&self, id: &RiftId) -> Result<Option<merge::BaseManifest>> {
        self.registry
            .base_manifest(id)?
            .map(|blob| merge::BaseManifest::decode(&blob))
            .transpose()
    }

    fn parent(&self, record: &Record, operation: &'static str) -> Result<Record> {
        let id = record.parent_id.clone().ok_or_else(|| Error::NoParent {
            path: record.path.clone(),
            operation,
        })?;
        self.registry
            .record_id(&id)?
            .ok_or_else(|| Error::NotManaged(record.path.clone()))
    }

    /// Reports which copy mechanism `create` would use for a path that exists
    /// on disk, and the filesystem type name when the platform reports it.
    pub fn probe(&self, at: impl AsRef<Path>) -> Result<Probe> {
        let path = existing_directory(at.as_ref())?;
        Ok(Probe {
            backend: self.strategy.probe(&path)?,
            filesystem: strategy::filesystem_name(&path),
            path,
        })
    }

    fn workspace_at(&self, path: impl AsRef<Path>) -> Result<Record> {
        let path = existing_directory(path.as_ref())?;
        self.workspace_from(&path)
    }

    fn workspace_from(&self, path: &Path) -> Result<Record> {
        self.workspace_from_optional(path)?
            .ok_or_else(|| Error::WorkspaceNotInitialized(path.to_path_buf()))
    }

    fn workspace_from_optional(&self, path: &Path) -> Result<Option<Record>> {
        for directory in path.ancestors() {
            if let Some(id) = marker::read(directory)? {
                let record = self
                    .registry
                    .record_id(&id)?
                    .ok_or_else(|| Error::UnknownMarker(directory.to_path_buf()))?;
                if record.path != directory {
                    return Err(Error::MarkerMismatch(directory.to_path_buf()));
                }
                return Ok(Some(record));
            }
            if self.registry.record_at(directory)?.is_some() {
                return Err(Error::MissingMarker(directory.to_path_buf()));
            }
        }
        Ok(None)
    }

    fn root(&self, record: &Record) -> Result<Record> {
        let mut current = record.clone();
        while let Some(id) = current.parent_id.clone() {
            current = self
                .registry
                .record_id(&id)?
                .ok_or_else(|| Error::NotManaged(record.path.clone()))?;
        }
        Ok(current)
    }

    /// Runs `f` holding the workspace family's merge lock. The lock is a
    /// registry row, so file work happens outside any SQLite transaction,
    /// and the row is released on every exit — Ok, Err, or unwinding —
    /// because a panic must not leave a live-pid row that locks the
    /// family for the rest of the process's lifetime.
    fn with_root_lock<T>(
        &mut self,
        root: &Record,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        if !self.registry.lock_root(&root.id)? {
            return Err(Error::Locked(root.path.clone()));
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        self.release_root(&root.id);
        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Best-effort lock release. A failed delete is never reported as the
    /// operation's error — the work already happened — and the leftover
    /// row is reclaimed once its pid dies. One immediate retry covers a
    /// transient database error.
    fn release_root(&mut self, root_id: &RiftId) {
        if self.registry.unlock_root(root_id).is_err() {
            let _ = self.registry.unlock_root(root_id);
        }
    }
}

fn default_database_path() -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .ok_or_else(|| Error::Path("user data directory is unavailable".into()))?;
    Ok(base.join("rift").join("rift.sqlite"))
}

fn existing_directory(path: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    if !path.is_dir() {
        return Err(Error::Path(format!("not a directory: {}", path.display())));
    }
    Ok(path)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn default_storage(root: &Path) -> Result<PathBuf> {
    let parent = root
        .parent()
        .ok_or_else(|| Error::Path(format!("workspace has no parent: {}", root.display())))?;
    let name = root
        .file_name()
        .ok_or_else(|| Error::Path(format!("workspace has no name: {}", root.display())))?;
    Ok(parent.join(".rifts").join(name))
}

fn trash_path(id: &RiftId, path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Path(format!("rift has no parent: {}", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("rift has no name: {}", path.display())))?;
    Ok(parent
        .join(".trash")
        .join(format!("{id}-{}", name.to_string_lossy())))
}

#[cfg(test)]
mod tests;
