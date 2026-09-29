use super::{Strategy, StrategyInit, create_destination};
use crate::{Backend, CopyMode, CowMode, Error, InitProgress, Result, filter::CopyFilter};
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

/// Copies with plain file reads and writes, so it works on every filesystem
/// and operating system. Used directly on platforms with no copy-on-write
/// backend, and by the platform strategies as a fallback when the filesystem
/// does not support their mechanism.
pub(crate) struct PortableStrategy;

impl Strategy for PortableStrategy {
    fn copy_directory(&self, from: &Path, to: &Path, mode: CopyMode, cow: CowMode) -> Result<()> {
        copy_or_require(from, to, mode, cow)
    }

    fn initialize_directory(
        &self,
        path: &Path,
        _progress: &mut dyn FnMut(InitProgress),
        cow: CowMode,
    ) -> Result<StrategyInit> {
        match cow {
            CowMode::Require => Err(Error::CowUnavailable(format!(
                "no copy-on-write method is available on this platform for {}",
                path.display()
            ))),
            CowMode::Auto => Ok(StrategyInit::Degraded),
        }
    }

    fn probe(&self, _path: &Path) -> Result<Backend> {
        Ok(Backend::Portable)
    }
}

/// Falls back to a regular copy, or refuses when the caller requires
/// copy-on-write.
pub(crate) fn copy_or_require(from: &Path, to: &Path, mode: CopyMode, cow: CowMode) -> Result<()> {
    if cow == CowMode::Require {
        return Err(Error::CowUnavailable(format!(
            "{} has no copy-on-write support; run without --cow-only to allow a regular copy",
            from.display()
        )));
    }
    copy_directory_portable(from, to, mode)
}

pub(crate) fn copy_directory_portable(from: &Path, to: &Path, mode: CopyMode) -> Result<()> {
    use std::collections::HashMap;

    create_destination(to)?;
    let mut hard_links: HashMap<(u64, u64), std::path::PathBuf> = HashMap::new();
    let mut directories = Vec::new();
    for entry in WalkDir::new(from)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            mode == CopyMode::All
                || entry
                    .path()
                    .strip_prefix(from)
                    .map_or(true, |path| !CopyFilter.excludes(path))
        })
    {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(
            source
                .strip_prefix(from)
                .map_err(|error| Error::Path(error.to_string()))?,
        );
        let metadata = fs::symlink_metadata(source)?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            fs::create_dir(&destination)?;
            directories.push((source.to_path_buf(), destination));
        } else if file_type.is_file() {
            let key = link_key(&metadata, source);
            if let Some(existing) = key.and_then(|key| hard_links.get(&key)) {
                fs::hard_link(existing, &destination)?;
                copy_metadata(source, &destination, MetadataTarget::FileOrDirectory)?;
            } else if is_git_object(from, source) && fs::hard_link(source, &destination).is_ok() {
                // Git objects are immutable, so a regular copy still shares
                // the object store's inodes instead of duplicating them —
                // on slow filesystems .git dominates the copy. When the two
                // trees sit on different filesystems the link fails and the
                // file is copied normally.
            } else {
                fs::copy(source, &destination)?;
                if let Some(key) = key {
                    hard_links.insert(key, destination.clone());
                }
                copy_metadata(source, &destination, MetadataTarget::FileOrDirectory)?;
            }
        } else if file_type.is_symlink() {
            create_symlink(source, &destination)?;
            copy_metadata(source, &destination, MetadataTarget::Symlink)?;
        } else {
            return Err(Error::UnsupportedEntry(source.to_path_buf()));
        }
    }
    for (source, destination) in directories.into_iter().rev() {
        copy_metadata(&source, &destination, MetadataTarget::FileOrDirectory)?;
    }
    copy_metadata(from, to, MetadataTarget::FileOrDirectory)
}

/// Files under `<from>/.git/objects` are Git's content-addressed object
/// store: immutable once written, so they can be hard-linked into a copy.
fn is_git_object(from: &Path, source: &Path) -> bool {
    source
        .strip_prefix(from)
        .is_ok_and(|relative| relative.starts_with(".git/objects"))
}

/// A stable identifier for deduplicating hard links within one copy.
fn link_key(metadata: &fs::Metadata, source: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = source;
        (metadata.nlink() > 1).then(|| (metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        by_handle_info(source).map(|info| {
            (
                u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (metadata, source);
        None
    }
}

/// `GetFileInformationByHandle` works on files and directories alike and
/// yields the volume serial number plus file index, which identify a file
/// within a volume. It is the stable-API equivalent of the unstable
/// `MetadataExt::volume_serial_number`/`file_index`.
#[cfg(windows)]
pub(crate) fn by_handle_info(
    path: &Path,
) -> Option<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    // SAFETY: `info` is a plain C struct the kernel fills in; `file` is an
    // open handle for the duration of the call.
    let mut info = unsafe { std::mem::zeroed() };
    // SAFETY: `file` is an open handle and `info` points at writable memory.
    (unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } != 0)
        .then_some(info)
}

/// Recreates a symlink, or copies the file it points at when the platform or
/// caller lacks the privilege to create links (common on Windows and on
/// filesystems without symlink support such as exFAT).
pub(crate) fn create_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source)?;
    create_symlink_at(&target, source, destination)
}

#[cfg(unix)]
fn create_symlink_at(target: &Path, _source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink_at(target: &Path, source: &Path, destination: &Path) -> Result<()> {
    let directory = fs::metadata(source).is_ok_and(|m| m.is_dir());
    let result = if directory {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            if fs::metadata(source).is_ok_and(|m| m.is_file()) {
                fs::copy(source, destination)?;
                return Ok(());
            }
            Err(error.into())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink_at(_target: &Path, source: &Path, destination: &Path) -> Result<()> {
    if fs::metadata(source).is_ok_and(|m| m.is_file()) {
        fs::copy(source, destination)?;
        return Ok(());
    }
    Err(Error::UnsupportedEntry(source.to_path_buf()))
}

#[derive(Clone, Copy)]
pub(crate) enum MetadataTarget {
    FileOrDirectory,
    Symlink,
}

/// Replays metadata on a plain copy. Ownership and extended attributes are
/// copied best-effort because they frequently require privileges the caller
/// lacks; permissions and timestamps remain strict.
pub(crate) fn copy_metadata(from: &Path, to: &Path, target: MetadataTarget) -> Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    copy_ownership_portable(from, to);
    copy_xattrs_portable(from, to);
    // Timestamps go on before permissions: `fs::copy` on Windows clones the
    // read-only attribute (common under `.git`), and a read-only destination
    // refuses new timestamps, so the bit is cleared first and the real
    // permissions applied last.
    if matches!(target, MetadataTarget::FileOrDirectory)
        && let Ok(existing) = fs::metadata(to)
    {
        let mut permissions = existing.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(to, permissions)?;
        }
    }
    copy_file_times(&metadata, to, target)?;
    if matches!(target, MetadataTarget::FileOrDirectory) {
        fs::set_permissions(to, metadata.permissions())?;
    }
    Ok(())
}

fn copy_file_times(metadata: &fs::Metadata, to: &Path, target: MetadataTarget) -> Result<()> {
    let atime = filetime::FileTime::from_last_access_time(metadata);
    let mtime = filetime::FileTime::from_last_modification_time(metadata);
    match target {
        MetadataTarget::FileOrDirectory => filetime::set_file_times(to, atime, mtime)?,
        MetadataTarget::Symlink => filetime::set_symlink_file_times(to, atime, mtime)?,
    }
    Ok(())
}

#[cfg(unix)]
fn copy_ownership_portable(from: &Path, to: &Path) {
    use std::os::unix::fs::MetadataExt;

    let Ok(metadata) = fs::symlink_metadata(from) else {
        return;
    };
    let Ok(destination) = c_path_portable(to) else {
        return;
    };
    // Best-effort: only a privileged caller can assign an owner or group it
    // does not belong to, and a copy owned by the caller is still correct.
    unsafe {
        libc::lchown(destination.as_ptr(), metadata.uid(), metadata.gid());
    }
}

#[cfg(not(unix))]
fn copy_ownership_portable(_from: &Path, _to: &Path) {}

#[cfg(target_os = "linux")]
fn copy_xattrs_portable(from: &Path, to: &Path) {
    let Ok(from) = c_path_portable(from) else {
        return;
    };
    let Ok(to) = c_path_portable(to) else {
        return;
    };
    // SAFETY: both paths are valid C strings. A null buffer with size 0 asks
    // the kernel for the required list size. Every call here is best-effort:
    // failures skip the attribute rather than aborting the copy.
    let size = unsafe { libc::llistxattr(from.as_ptr(), std::ptr::null_mut(), 0) };
    if size <= 0 {
        return;
    }
    let mut names = vec![0_u8; size as usize];
    // SAFETY: `names` is sized to the list reported above and valid for writes.
    if unsafe { libc::llistxattr(from.as_ptr(), names.as_mut_ptr().cast(), names.len()) } < 0 {
        return;
    }
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let Ok(name) = std::ffi::CString::new(name) else {
            continue;
        };
        // SAFETY: `from` and `name` are valid C strings; a null buffer asks
        // for the attribute's value length.
        let size =
            unsafe { libc::lgetxattr(from.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            continue;
        }
        let mut value = vec![0_u8; size as usize];
        // SAFETY: `value` is sized to the reported value length.
        if size > 0
            && unsafe {
                libc::lgetxattr(
                    from.as_ptr(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                )
            } < 0
        {
            continue;
        }
        // SAFETY: `to`, `name`, and `value` are all valid for the call.
        unsafe {
            libc::lsetxattr(
                to.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn copy_xattrs_portable(from: &Path, to: &Path) {
    let Ok(from) = c_path_portable(from) else {
        return;
    };
    let Ok(to) = c_path_portable(to) else {
        return;
    };
    // SAFETY: same contract as the Linux path above; XATTR_NOFOLLOW keeps
    // symlink behavior consistent. All failures are skipped best-effort.
    let size =
        unsafe { libc::listxattr(from.as_ptr(), std::ptr::null_mut(), 0, libc::XATTR_NOFOLLOW) };
    if size <= 0 {
        return;
    }
    let mut names = vec![0_u8; size as usize];
    if unsafe {
        libc::listxattr(
            from.as_ptr(),
            names.as_mut_ptr().cast(),
            names.len(),
            libc::XATTR_NOFOLLOW,
        )
    } < 0
    {
        return;
    }
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let Ok(name) = std::ffi::CString::new(name) else {
            continue;
        };
        let size = unsafe {
            libc::getxattr(
                from.as_ptr(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        if size < 0 {
            continue;
        }
        let mut value = vec![0_u8; size as usize];
        if size > 0
            && unsafe {
                libc::getxattr(
                    from.as_ptr(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            } < 0
        {
            continue;
        }
        unsafe {
            libc::setxattr(
                to.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            );
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn copy_xattrs_portable(_from: &Path, _to: &Path) {}

#[cfg(unix)]
fn c_path_portable(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn portable_copy_creates_a_regular_copy() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "hello").unwrap();
        fs::write(source.join("other.txt"), "world").unwrap();

        PortableStrategy
            .copy_directory(&source, &destination, CopyMode::All, CowMode::Auto)
            .unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "hello"
        );
        fs::write(source.join("nested/file.txt"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "hello"
        );
        PortableStrategy.remove_directory(&destination).unwrap();
        assert!(!destination.exists());
    }

    #[test]
    fn portable_copy_applies_the_artifact_filter() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::write(source.join("node_modules/pkg/index.js"), "module").unwrap();
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(source.join("package.json"), "{}").unwrap();
        let destination = temp.path().join("destination");

        PortableStrategy
            .copy_directory(&source, &destination, CopyMode::Filtered, CowMode::Auto)
            .unwrap();

        assert!(!destination.join("node_modules").exists());
        assert_eq!(
            fs::read_to_string(destination.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
    }

    #[test]
    fn portable_copy_preserves_hard_links() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), "linked").unwrap();
        if fs::hard_link(source.join("file.txt"), source.join("hard.txt")).is_err() {
            return;
        }
        let destination = temp.path().join("destination");

        PortableStrategy
            .copy_directory(&source, &destination, CopyMode::All, CowMode::Auto)
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(destination.join("file.txt")).unwrap().ino(),
                fs::metadata(destination.join("hard.txt")).unwrap().ino()
            );
        }
        assert_eq!(
            fs::read_to_string(destination.join("hard.txt")).unwrap(),
            "linked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn portable_copy_preserves_symlinks_permissions_and_times() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let file = source.join("file.txt");
        fs::write(&file, "hello").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        let past = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_times(&file, past, past).unwrap();
        std::os::unix::fs::symlink("file.txt", source.join("link.txt")).unwrap();
        let destination = temp.path().join("destination");

        PortableStrategy
            .copy_directory(&source, &destination, CopyMode::All, CowMode::Auto)
            .unwrap();

        assert_eq!(
            fs::read_link(destination.join("link.txt")).unwrap(),
            Path::new("file.txt")
        );
        assert_eq!(
            fs::metadata(destination.join("file.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            filetime::FileTime::from_last_modification_time(
                &fs::metadata(destination.join("file.txt")).unwrap()
            ),
            past
        );
    }

    #[test]
    fn portable_copy_rejects_special_entries() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;

            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            fs::create_dir_all(&source).unwrap();
            let fifo = source.join("pipe");
            if unsafe { libc::mkfifo(c_path_portable(&fifo).unwrap().as_ptr(), 0o644) } != 0 {
                return;
            }
            assert!(
                fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo(),
                "fixture did not create a fifo"
            );
            let destination = temp.path().join("destination");

            assert!(matches!(
                PortableStrategy.copy_directory(
                    &source,
                    &destination,
                    CopyMode::All,
                    CowMode::Auto
                ),
                Err(Error::UnsupportedEntry(_))
            ));
        }
    }

    #[test]
    fn require_mode_refuses_a_plain_copy() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let destination = temp.path().join("destination");

        assert!(matches!(
            PortableStrategy.copy_directory(&source, &destination, CopyMode::All, CowMode::Require),
            Err(Error::CowUnavailable(_))
        ));
        assert!(!destination.exists());
        assert!(matches!(
            PortableStrategy.initialize_directory(&source, &mut |_| {}, CowMode::Require),
            Err(Error::CowUnavailable(_))
        ));
        assert_eq!(
            PortableStrategy
                .initialize_directory(&source, &mut |_| {}, CowMode::Auto)
                .unwrap(),
            StrategyInit::Degraded
        );
    }
}
