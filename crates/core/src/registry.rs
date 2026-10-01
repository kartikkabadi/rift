use crate::{Error, Result, id::RiftId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct Record {
    pub(crate) id: RiftId,
    pub(crate) parent_id: Option<RiftId>,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathRecord {
    pub(crate) id: RiftId,
    pub(crate) path: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct MovedRecord {
    pub(crate) id: RiftId,
    pub(crate) original_path: PathBuf,
    pub(crate) trash_path: PathBuf,
}

#[derive(Clone, Copy)]
pub(crate) enum SubtreeScope {
    IncludingRoot,
    DescendantsOnly,
}

impl SubtreeScope {
    fn min_depth(self) -> u8 {
        match self {
            Self::IncludingRoot => 0,
            Self::DescendantsOnly => 1,
        }
    }
}

pub(crate) struct Registry {
    database: Connection,
}

impl Registry {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let database = Connection::open(path)?;
        database.execute_batch(
            "PRAGMA busy_timeout = 2000;
             PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS rift (
               id TEXT PRIMARY KEY,
               parent_id TEXT REFERENCES rift(id) ON DELETE CASCADE,
               path TEXT NOT NULL UNIQUE,
               created_at INTEGER NOT NULL
              );
              CREATE INDEX IF NOT EXISTS rift_parent_id_idx ON rift(parent_id);
              CREATE TABLE IF NOT EXISTS trash (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                removed_at INTEGER NOT NULL
              );
              CREATE TABLE IF NOT EXISTS rift_base (
                rift_id TEXT PRIMARY KEY REFERENCES rift(id) ON DELETE CASCADE,
                base_manifest BLOB NOT NULL,
                base_head TEXT,
                updated_at INTEGER NOT NULL
              );
              CREATE TABLE IF NOT EXISTS land_locks (
                root_id TEXT PRIMARY KEY REFERENCES rift(id) ON DELETE CASCADE,
                pid INTEGER NOT NULL,
                started_at INTEGER NOT NULL
              );",
        )?;
        Ok(Self { database })
    }

    pub(crate) fn insert_root(&self, id: &RiftId, path: &Path) -> Result<()> {
        self.database.execute(
            "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, NULL, ?2, ?3)",
            params![id.as_str(), path_text(path)?, timestamp()],
        )?;
        Ok(())
    }

    /// Registers a rift and its base manifest atomically: a rift row must
    /// never exist without the base its `land`/`sync` merges against.
    pub(crate) fn insert_child_with_base(
        &mut self,
        id: &RiftId,
        parent_id: &RiftId,
        path: &Path,
        base_manifest: &[u8],
        base_head: Option<&str>,
    ) -> Result<()> {
        let transaction = self.database.transaction()?;
        transaction.execute(
            "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                id.as_str(),
                parent_id.as_str(),
                path_text(path)?,
                timestamp()
            ],
        )?;
        transaction.execute(
            "INSERT INTO rift_base (rift_id, base_manifest, base_head, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![id.as_str(), base_manifest, base_head, timestamp()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn base_manifest(&self, id: &RiftId) -> Result<Option<Vec<u8>>> {
        self.database
            .query_row(
                "SELECT base_manifest FROM rift_base WHERE rift_id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Error::from)
    }

    /// Replaces the recorded base after a `land` or `sync` advanced it.
    pub(crate) fn update_base(&self, id: &RiftId, base_manifest: &[u8]) -> Result<()> {
        self.database.execute(
            "UPDATE rift_base SET base_manifest = ?2, updated_at = ?3 WHERE rift_id = ?1",
            params![id.as_str(), base_manifest, timestamp()],
        )?;
        Ok(())
    }

    /// Acquires the root workspace's merge lock: one row per root, so file
    /// work for `create`, `land`, `sync`, and `remove` never interleaves on
    /// the same workspace family. Returns `false` when a live process
    /// already holds it; a row left by a dead process is reclaimed.
    pub(crate) fn lock_root(&mut self, root_id: &RiftId) -> Result<bool> {
        let transaction = self.database.transaction()?;
        if try_acquire(&transaction, root_id)? {
            transaction.commit()?;
            return Ok(true);
        }
        let owner: Option<i64> = transaction
            .query_row(
                "SELECT pid FROM land_locks WHERE root_id = ?1",
                [root_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(owner) = owner else {
            // The holder released between the failed insert and the check.
            let acquired = try_acquire(&transaction, root_id)?;
            transaction.commit()?;
            return Ok(acquired);
        };
        if pid_alive(owner as u32) {
            transaction.rollback()?;
            return Ok(false);
        }
        transaction.execute(
            "DELETE FROM land_locks WHERE root_id = ?1 AND pid = ?2",
            params![root_id.as_str(), owner],
        )?;
        let acquired = try_acquire(&transaction, root_id)?;
        // Committing releases this transaction's writes either way; a
        // failed acquire only means a live competitor took the row first.
        transaction.commit()?;
        Ok(acquired)
    }

    /// Releases this process's lock on the root workspace.
    pub(crate) fn unlock_root(&self, root_id: &RiftId) -> Result<()> {
        self.database.execute(
            "DELETE FROM land_locks WHERE root_id = ?1 AND pid = ?2",
            params![root_id.as_str(), std::process::id() as i64],
        )?;
        Ok(())
    }

    pub(crate) fn record_at(&self, path: &Path) -> Result<Option<Record>> {
        self.database
            .query_row(
                "SELECT id, parent_id, path FROM rift WHERE path = ?1",
                [path_text(path)?],
                record_from_row,
            )
            .optional()
            .map_err(Error::from)
    }

    pub(crate) fn record_id(&self, id: &RiftId) -> Result<Option<Record>> {
        self.database
            .query_row(
                "SELECT id, parent_id, path FROM rift WHERE id = ?1",
                [id.as_str()],
                record_from_row,
            )
            .optional()
            .map_err(Error::from)
    }

    pub(crate) fn subtree(&self, id: &RiftId, scope: SubtreeScope) -> Result<Vec<PathRecord>> {
        let mut statement = self.database.prepare(
            "WITH RECURSIVE subtree(id, path, depth) AS (
               SELECT id, path, 0 FROM rift WHERE id = ?1
               UNION ALL
               SELECT rift.id, rift.path, subtree.depth + 1
               FROM rift JOIN subtree ON rift.parent_id = subtree.id
             ) SELECT id, path FROM subtree WHERE depth >= ?2 ORDER BY depth DESC, id",
        )?;
        let rows = statement
            .query_map(params![id.as_str(), scope.min_depth()], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn child_paths(&self, parent_id: &RiftId) -> Result<Vec<PathBuf>> {
        let mut statement = self
            .database
            .prepare("SELECT path FROM rift WHERE parent_id = ?1 ORDER BY created_at, id")?;
        Ok(statement
            .query_map([parent_id.as_str()], |row| {
                Ok(PathBuf::from(row.get::<_, String>(0)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delete_active(&self, id: &RiftId) -> Result<()> {
        self.database
            .execute("DELETE FROM rift WHERE id = ?1", [id.as_str()])?;
        Ok(())
    }

    pub(crate) fn trash_moved(&mut self, moved: &[MovedRecord]) -> Result<()> {
        let transaction = self.database.transaction()?;
        moved.iter().try_for_each(|record| -> Result<()> {
            transaction.execute(
                "INSERT INTO trash (id, path, removed_at) VALUES (?1, ?2, ?3)",
                params![
                    record.id.as_str(),
                    path_text(&record.trash_path)?,
                    timestamp()
                ],
            )?;
            transaction.execute("DELETE FROM rift WHERE id = ?1", [record.id.as_str()])?;
            Ok(())
        })?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn trashed_paths(&self) -> Result<Vec<PathRecord>> {
        let mut statement = self
            .database
            .prepare("SELECT id, path FROM trash ORDER BY removed_at, id")?;
        Ok(statement
            .query_map([], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delete_trash(&self, id: &RiftId) -> Result<()> {
        self.database
            .execute("DELETE FROM trash WHERE id = ?1", [id.as_str()])?;
        Ok(())
    }

    /// Every registered root workspace, for `gc`'s storage sweep.
    pub(crate) fn root_paths(&self) -> Result<Vec<PathRecord>> {
        let mut statement = self
            .database
            .prepare("SELECT id, path FROM rift WHERE parent_id IS NULL")?;
        Ok(statement
            .query_map([], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn active_paths(&self) -> Result<Vec<PathRecord>> {
        let mut statement = self.database.prepare("SELECT id, path FROM rift")?;
        Ok(statement
            .query_map([], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delete_active_records(&mut self, rows: &[PathRecord]) -> Result<()> {
        let transaction = self.database.transaction()?;
        rows.iter().try_for_each(|record| -> Result<()> {
            transaction.execute("DELETE FROM rift WHERE id = ?1", [record.id.as_str()])?;
            Ok(())
        })?;
        transaction.commit()?;
        Ok(())
    }
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: RiftId::from_stored(row.get(0)?),
        parent_id: row.get::<_, Option<String>>(1)?.map(RiftId::from_stored),
        path: PathBuf::from(row.get::<_, String>(2)?),
    })
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| Error::Path(format!("path is not valid UTF-8: {}", path.display())))
}

fn timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Inserts this process's lock row. `false` means a lock row already
/// exists — the primary key is the mutual exclusion. A foreign-key
/// violation means the root itself was deleted mid-operation and is a
/// real error, not a held lock.
fn try_acquire(transaction: &rusqlite::Transaction<'_>, root_id: &RiftId) -> Result<bool> {
    match transaction.execute(
        "INSERT INTO land_locks (root_id, pid, started_at) VALUES (?1, ?2, ?3)",
        params![root_id.as_str(), std::process::id() as i64, timestamp()],
    ) {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::SqliteFailure(failure, _))
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        // Pids this large cannot be probed (or wrap into signal-all
        // values); treat them as a dead or corrupt lock owner.
        return false;
    }
    // Signal 0 probes existence; EPERM still means the process exists.
    (unsafe { libc::kill(pid as i32, 0) } == 0)
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: `pid` comes from the lock row and the returned handle is
    // closed immediately. A dead pid simply fails to open.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    let null = std::ptr::null_mut::<std::ffi::c_void>() as _;
    if handle == null {
        return false;
    }
    unsafe { CloseHandle(handle) };
    true
}

#[cfg(not(any(unix, windows)))]
fn pid_alive(_pid: u32) -> bool {
    // No portable liveness probe: assume the owner lives rather than
    // reclaiming a lock out from under it.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registry() -> (TempDir, Registry) {
        let temp = TempDir::new().unwrap();
        let registry = Registry::open(temp.path().join("registry.sqlite")).unwrap();
        (temp, registry)
    }

    fn empty_base() -> Vec<u8> {
        crate::merge::BaseManifest::empty().encode()
    }

    /// A pid guaranteed dead: run a process that exits at once and keep
    /// its id.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn root_lock_excludes_a_second_connection_and_reclaims_a_dead_owner() {
        let (temp, mut registry) = registry();
        let root = temp.path().join("root");
        let root_id = id("root");
        registry.insert_root(&root_id, &root).unwrap();

        assert!(registry.lock_root(&root_id).unwrap());
        // A second connection to the same registry sees the lock held by a
        // live owner (this process) and cannot take it.
        let mut other = Registry::open(temp.path().join("registry.sqlite")).unwrap();
        assert!(!other.lock_root(&root_id).unwrap());

        registry.unlock_root(&root_id).unwrap();
        assert!(other.lock_root(&root_id).unwrap());
        other.unlock_root(&root_id).unwrap();

        // A lock row owned by a dead pid is stale and is reclaimed.
        registry
            .database
            .execute(
                "INSERT INTO land_locks (root_id, pid, started_at) VALUES (?1, ?2, 0)",
                params![root_id.as_str(), dead_pid() as i64],
            )
            .unwrap();
        assert!(registry.lock_root(&root_id).unwrap());
        registry.unlock_root(&root_id).unwrap();
    }

    #[test]
    fn removing_a_rift_cascades_its_lock_row() {
        let (temp, mut registry) = registry();
        let root = temp.path().join("root");
        let root_id = id("root");
        registry.insert_root(&root_id, &root).unwrap();
        registry.lock_root(&root_id).unwrap();

        registry
            .database
            .execute("DELETE FROM rift WHERE id = ?1", [root_id.as_str()])
            .unwrap();

        // Deleting the root row cascades the lock row away with it.
        let locks: i64 = registry
            .database
            .query_row("SELECT COUNT(*) FROM land_locks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(locks, 0);
    }

    #[test]
    fn uses_wal_and_busy_timeout() {
        let (_temp, registry) = registry();
        let journal_mode: String = registry
            .database
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let busy_timeout: i32 = registry
            .database
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        assert_eq!(journal_mode, "wal");
        assert_eq!(busy_timeout, 2000);
    }

    #[test]
    fn subtree_returns_descendants_before_ancestors() {
        let (temp, mut registry) = registry();
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let sibling = temp.path().join("sibling");
        let grandchild = temp.path().join("grandchild");
        let root_id = id("root");
        let child_id = id("child");
        let sibling_id = id("sibling");
        let grandchild_id = id("grandchild");
        registry.insert_root(&root_id, &root).unwrap();
        registry
            .insert_child_with_base(&child_id, &root_id, &child, &empty_base(), None)
            .unwrap();
        registry
            .insert_child_with_base(&sibling_id, &root_id, &sibling, &empty_base(), None)
            .unwrap();
        registry
            .insert_child_with_base(&grandchild_id, &child_id, &grandchild, &empty_base(), None)
            .unwrap();

        let subtree = registry
            .subtree(&root_id, SubtreeScope::IncludingRoot)
            .unwrap()
            .into_iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>();
        let descendants = registry
            .subtree(&root_id, SubtreeScope::DescendantsOnly)
            .unwrap()
            .into_iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>();

        assert_eq!(subtree, vec!["grandchild", "child", "sibling", "root"]);
        assert_eq!(descendants, vec!["grandchild", "child", "sibling"]);
        assert_eq!(
            registry.child_paths(&root_id).unwrap(),
            vec![child, sibling]
        );
    }

    #[test]
    fn trash_moved_transfers_records_from_active_tree_to_trash() {
        let (temp, mut registry) = registry();
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let trash = temp.path().join(".trash/child");
        let root_id = id("root");
        let child_id = id("child");
        registry.insert_root(&root_id, &root).unwrap();
        registry
            .insert_child_with_base(&child_id, &root_id, &child, &empty_base(), None)
            .unwrap();

        registry
            .trash_moved(&[MovedRecord {
                id: child_id.clone(),
                original_path: child.clone(),
                trash_path: trash.clone(),
            }])
            .unwrap();

        assert!(registry.record_id(&root_id).unwrap().is_some());
        assert!(registry.record_id(&child_id).unwrap().is_none());
        assert_eq!(
            registry.trashed_paths().unwrap(),
            vec![PathRecord {
                id: child_id,
                path: trash,
            }]
        );
    }

    fn id(value: &str) -> RiftId {
        RiftId::from_stored(value.to_owned())
    }
}
