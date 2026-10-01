use super::{
    Strategy, StrategyInit, create_destination,
    portable::{self, MetadataTarget, PortableStrategy},
};
use crate::{Backend, CopyMode, CowMode, Error, InitProgress, Result, filter::CopyFilter};
use std::fs;
use std::path::Path;
use walkdir::WalkDir;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE};

pub(super) struct WindowsStrategy;

impl Strategy for WindowsStrategy {
    fn copy_directory(&self, from: &Path, to: &Path, mode: CopyMode, cow: CowMode) -> Result<()> {
        let destination_parent = to
            .parent()
            .ok_or_else(|| Error::Path(format!("destination has no parent: {}", to.display())))?;
        if !same_volume(from, destination_parent)? {
            return match cow {
                CowMode::Require => Err(Error::CowUnavailable(format!(
                    "copy-on-write copies require the source and destination on the same volume: {}",
                    to.display()
                ))),
                CowMode::Auto => PortableStrategy.copy_directory(from, to, mode, cow),
            };
        }
        match verify_block_clone(destination_parent) {
            Ok(()) => match mode {
                CopyMode::All => clone_directory_windows(from, to, None),
                CopyMode::Filtered => clone_filtered_directory_windows(from, to),
            },
            Err(error) if cow == CowMode::Require => Err(error),
            Err(_) => PortableStrategy.copy_directory(from, to, mode, cow),
        }
    }

    fn initialize_directory(
        &self,
        path: &Path,
        _progress: &mut dyn FnMut(InitProgress),
        cow: CowMode,
    ) -> Result<StrategyInit> {
        match verify_block_clone(path) {
            Ok(()) => Ok(StrategyInit::AlreadyNative),
            Err(error) if cow == CowMode::Require => Err(error),
            Err(_) => Ok(StrategyInit::Degraded),
        }
    }

    fn probe(&self, path: &Path) -> Result<Backend> {
        if verify_block_clone(path).is_ok() {
            return Ok(Backend::ReFs);
        }
        Ok(Backend::Portable)
    }
}

fn same_volume(from: &Path, destination_parent: &Path) -> Result<bool> {
    Ok(
        match (
            portable::by_handle_info(from),
            portable::by_handle_info(destination_parent),
        ) {
            (Some(from), Some(destination)) => {
                from.dwVolumeSerialNumber == destination.dwVolumeSerialNumber
            }
            // When the volume cannot be identified, assume it differs: an Auto
            // caller still gets a working portable copy.
            _ => false,
        },
    )
}

/// The volume's filesystem name ("NTFS", "ReFS", ...), via the open handle so
/// mounted volumes inside directories report correctly.
pub(super) fn filesystem_name(path: &Path) -> Option<String> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = fs::OpenOptions::new()
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut name = vec![0_u16; 64];
    // SAFETY: `file` is an open handle, `name` is a valid writable buffer of
    // `name.len()` wide characters, and the optional out-parameters are null.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle() as _,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let length = name.iter().position(|c| *c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..length]))
}

fn cluster_size(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceW;

    let root = path
        .ancestors()
        .last()
        .ok_or_else(|| Error::Path(format!("path has no volume root: {}", path.display())))?;
    let root_wide: Vec<u16> = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sectors_per_cluster = 0;
    let mut bytes_per_sector = 0;
    // SAFETY: `root_wide` is a null-terminated wide string and the
    // out-parameters point at valid locals.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            root_wide.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(u64::from(sectors_per_cluster) * u64::from(bytes_per_sector))
}

/// ReFS reports a 4 KiB cluster even when a volume uses another size; probing
/// avoids relying on the reported size at all where a simple write works.
fn verify_block_clone(path: &Path) -> Result<()> {
    let operation_id = ulid::Ulid::new();
    let source = path.join(format!(".rift-clone-probe-{operation_id}"));
    let destination = path.join(format!(".rift-clone-probe-copy-{operation_id}"));
    let cluster = cluster_size(path).unwrap_or(4096);
    fs::write(&source, vec![0_u8; cluster as usize])?;
    let result = clone_file_windows(&source, &destination, cluster).map_err(|error| match error {
        Error::CowUnavailable(message) => Error::CowUnavailable(format!(
            "{} does not support ReFS block cloning: {message}",
            path.display()
        )),
        error => error,
    });
    let cleanup = [&source, &destination]
        .into_iter()
        .filter(|path| path.exists())
        .try_for_each(fs::remove_file);
    result.and(cleanup.map_err(Error::from))
}

/// Duplicates `from` into a fresh file at `to` using ReFS block cloning. The
/// final partial cluster cannot be cloned (block regions are
/// cluster-aligned), so the tail is written the ordinary way.
fn clone_file_windows(from: &Path, to: &Path, cluster: u64) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::windows::io::AsRawHandle;

    let mut source = fs::File::open(from)?;
    let size = source.metadata()?.len();
    let mut destination = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    let aligned = size - size % cluster;
    if aligned > 0 {
        destination.set_len(aligned)?;
        let mut extents = DUPLICATE_EXTENTS_DATA {
            FileHandle: source.as_raw_handle() as _,
            SourceFileOffset: 0,
            TargetFileOffset: 0,
            ByteCount: aligned as i64,
        };
        // SAFETY: `destination` is an open writable handle, `extents` lives
        // for the call, and the source handle stays open on `source`.
        let ok = unsafe {
            DeviceIoControl(
                destination.as_raw_handle() as _,
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                &mut extents as *mut _ as *const _,
                size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::CowUnavailable(format!(
                "failed to block-clone {}: {}",
                from.display(),
                std::io::Error::last_os_error()
            )));
        }
    }
    if aligned < size {
        source.seek(SeekFrom::Start(aligned))?;
        let mut tail = Vec::new();
        source.read_to_end(&mut tail)?;
        destination.seek(SeekFrom::Start(aligned))?;
        destination.write_all(&tail)?;
    }
    destination.set_len(size)?;
    Ok(())
}

fn clone_directory_windows(from: &Path, to: &Path, cluster: Option<u64>) -> Result<()> {
    create_destination(to)?;
    let cluster = match cluster {
        Some(cluster) => cluster,
        None => cluster_size(from)?,
    };
    let mut directories = Vec::new();
    let mut hard_links = std::collections::HashMap::new();
    for entry in WalkDir::new(from).min_depth(1).follow_links(false) {
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
            clone_or_link(source, &destination, cluster, &mut hard_links)?;
            copy_metadata(source, &destination, MetadataTarget::FileOrDirectory)?;
        } else if file_type.is_symlink() {
            portable::create_symlink(source, &destination)?;
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

fn clone_filtered_directory_windows(from: &Path, to: &Path) -> Result<()> {
    create_destination(to)?;
    let cluster = cluster_size(from)?;
    let filter = CopyFilter;
    let mut directories = Vec::new();
    let mut hard_links = std::collections::HashMap::new();
    for entry in WalkDir::new(from)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry
                .path()
                .strip_prefix(from)
                .map_or(true, |path| !filter.excludes(path))
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
            clone_or_link(source, &destination, cluster, &mut hard_links)?;
            copy_metadata(source, &destination, MetadataTarget::FileOrDirectory)?;
        } else if file_type.is_symlink() {
            portable::create_symlink(source, &destination)?;
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

fn copy_metadata(from: &Path, to: &Path, target: MetadataTarget) -> Result<()> {
    portable::copy_metadata(from, to, target)
}

/// Block-clones one file, or links it to its already-cloned sibling when the
/// source shares storage through multiple hard links.
fn clone_or_link(
    source: &Path,
    destination: &Path,
    cluster: u64,
    hard_links: &mut std::collections::HashMap<(u64, u64), std::path::PathBuf>,
) -> Result<()> {
    let key = portable::by_handle_info(source).and_then(|info| {
        (info.nNumberOfLinks > 1).then(|| {
            (
                u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            )
        })
    });
    if let Some(existing) = key.and_then(|key| hard_links.get(&key)) {
        fs::hard_link(existing, destination)?;
        return Ok(());
    }
    clone_file_windows(source, destination, cluster)?;
    if let Some(key) = key {
        hard_links.insert(key, destination.to_path_buf());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn clone_or_fallback_copies_a_tree() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "hello").unwrap();

        WindowsStrategy
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
    }
}
