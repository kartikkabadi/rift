use crate::{Backend, CopyMode, CowMode, InitProgress, Result};
#[cfg(test)]
use crate::{Error, filter::CopyFilter};
use std::fs;
use std::io;
use std::path::Path;

#[cfg(target_os = "macos")]
mod apfs;
#[cfg(target_os = "linux")]
mod btrfs;
#[cfg(target_os = "linux")]
mod linux;
pub(crate) mod portable;
#[cfg(target_os = "linux")]
mod reflink;
#[cfg(target_os = "windows")]
mod windows;

pub(crate) trait Strategy {
    /// Copies `from` to `to`. When `cow` is `CowMode::Require`, filesystems
    /// without a copy-on-write mechanism fail instead of copying normally.
    fn copy_directory(&self, from: &Path, to: &Path, mode: CopyMode, cow: CowMode) -> Result<()>;

    fn initialize_directory(
        &self,
        _path: &Path,
        _progress: &mut dyn FnMut(InitProgress),
        _cow: CowMode,
    ) -> Result<StrategyInit> {
        Ok(StrategyInit::AlreadyNative)
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        fs::remove_dir_all(path)?;
        Ok(())
    }

    /// Reports which copy mechanism `copy_directory` would use for `path`.
    fn probe(&self, path: &Path) -> Result<Backend>;
}

fn create_destination(path: &Path) -> Result<()> {
    fs::create_dir(path).map_err(|error| match error.kind() {
        io::ErrorKind::AlreadyExists => crate::Error::AlreadyExists(path.to_path_buf()),
        _ => error.into(),
    })
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StrategyInit {
    AlreadyNative,
    Converted,
    /// The workspace registered, but the filesystem cannot copy-on-write, so
    /// workspaces created from it will be regular copies.
    Degraded,
}

pub(crate) fn default_strategy() -> Box<dyn Strategy> {
    #[cfg(target_os = "linux")]
    return Box::new(linux::LinuxStrategy);

    #[cfg(target_os = "macos")]
    return Box::new(apfs::ApfsStrategy);

    #[cfg(target_os = "windows")]
    return Box::new(windows::WindowsStrategy);

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    return Box::new(portable::PortableStrategy);
}

/// The filesystem type name at `path`, when the platform can report it.
pub(crate) fn filesystem_name(path: &Path) -> Option<String> {
    #[cfg(target_os = "linux")]
    return linux::filesystem_name(path);
    #[cfg(target_os = "macos")]
    return apfs::filesystem_name(path);
    #[cfg(target_os = "windows")]
    return windows::filesystem_name(path);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = path;
        None
    }
}

#[cfg(all(test, unix))]
fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    Ok(())
}

#[cfg(all(test, windows))]
fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
    let target = fs::read_link(from)?;
    if fs::metadata(from)?.is_dir() {
        std::os::windows::fs::symlink_dir(target, to)?;
        return Ok(());
    }
    std::os::windows::fs::symlink_file(target, to)?;
    Ok(())
}

#[cfg(test)]
pub(crate) struct TestStrategy;

#[cfg(test)]
impl Strategy for TestStrategy {
    fn copy_directory(&self, from: &Path, to: &Path, mode: CopyMode, _cow: CowMode) -> Result<()> {
        create_destination(to)?;
        let filter = CopyFilter;
        for entry in walkdir::WalkDir::new(from)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                mode == CopyMode::All
                    || entry
                        .path()
                        .strip_prefix(from)
                        .map_or(true, |path| !filter.excludes(path))
            })
        {
            let entry = entry?;
            let destination = to.join(
                entry
                    .path()
                    .strip_prefix(from)
                    .map_err(|error| Error::Path(error.to_string()))?,
            );
            if entry.file_type().is_dir() {
                fs::create_dir(&destination)?;
                continue;
            }
            if entry.file_type().is_symlink() {
                copy_symlink(entry.path(), &destination)?;
                continue;
            }
            fs::copy(entry.path(), destination)?;
        }
        Ok(())
    }

    fn probe(&self, _path: &Path) -> Result<Backend> {
        Ok(Backend::Portable)
    }
}

#[cfg(test)]
pub(crate) struct FailureStrategy;

#[cfg(test)]
impl Strategy for FailureStrategy {
    fn copy_directory(
        &self,
        _from: &Path,
        _to: &Path,
        _mode: CopyMode,
        _cow: CowMode,
    ) -> Result<()> {
        Err(Error::CowUnavailable("test failure".into()))
    }

    fn probe(&self, _path: &Path) -> Result<Backend> {
        Ok(Backend::Portable)
    }
}
