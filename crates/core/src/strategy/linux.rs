use super::{
    Strategy, StrategyInit,
    btrfs::BtrfsStrategy,
    portable::PortableStrategy,
    reflink::{LinuxReflinkStrategy, verify_reflinks_linux},
};
use crate::{Backend, CopyMode, CowMode, Error, InitProgress, Result};
use std::fs;
use std::path::Path;

pub(super) struct LinuxStrategy;

impl Strategy for LinuxStrategy {
    fn copy_directory(&self, from: &Path, to: &Path, mode: CopyMode, cow: CowMode) -> Result<()> {
        let destination_parent = to
            .parent()
            .ok_or_else(|| Error::Path(format!("destination has no parent: {}", to.display())))?;
        match (
            filesystem(from)?,
            same_filesystem(from, destination_parent)?,
        ) {
            (Filesystem::Btrfs, true) => BtrfsStrategy.copy_directory(from, to, mode, cow),
            (Filesystem::Other, true) => match verify_reflinks_linux(destination_parent) {
                Ok(()) => LinuxReflinkStrategy.copy_directory(from, to, mode, cow),
                Err(error) if cow == CowMode::Require => Err(error),
                Err(_) => PortableStrategy.copy_directory(from, to, mode, cow),
            },
            (_, false) => match cow {
                CowMode::Require => Err(Error::CowUnavailable(format!(
                    "copy-on-write copies require the source and destination on the same filesystem: {}",
                    to.display()
                ))),
                CowMode::Auto => PortableStrategy.copy_directory(from, to, mode, cow),
            },
        }
    }

    fn initialize_directory(
        &self,
        path: &Path,
        progress: &mut dyn FnMut(InitProgress),
        cow: CowMode,
    ) -> Result<StrategyInit> {
        match filesystem(path)? {
            Filesystem::Btrfs => BtrfsStrategy.initialize_directory(path, progress, cow),
            Filesystem::Other => match verify_reflinks_linux(path) {
                Ok(()) => Ok(StrategyInit::AlreadyNative),
                Err(error) if cow == CowMode::Require => Err(error),
                Err(_) => Ok(StrategyInit::Degraded),
            },
        }
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        match filesystem(path)? {
            Filesystem::Btrfs => BtrfsStrategy.remove_directory(path),
            Filesystem::Other => {
                fs::remove_dir_all(path)?;
                Ok(())
            }
        }
    }

    fn probe(&self, path: &Path) -> Result<Backend> {
        match filesystem(path)? {
            Filesystem::Btrfs => Ok(Backend::Btrfs),
            Filesystem::Other if verify_reflinks_linux(path).is_ok() => Ok(Backend::Reflink),
            Filesystem::Other => Ok(Backend::Portable),
        }
    }
}

// Comparing `f_fsid`, not `st_dev`: on btrfs every subvolume reports its own
// anonymous st_dev, so an initialized workspace (a subvolume) and the `.rifts`
// storage beside it look like different filesystems even though snapshots
// between them are legal. `f_fsid` identifies the mounted filesystem itself —
// identical across its subvolumes and different across separate mounts. libc
// keeps `fsid_t`'s fields private, so compare through its derived Debug.
fn same_filesystem(from: &Path, destination_parent: &Path) -> Result<bool> {
    Ok(format!("{:?}", statfs(from)?.f_fsid)
        == format!("{:?}", statfs(destination_parent)?.f_fsid))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Filesystem {
    Btrfs,
    Other,
}

fn statfs(path: &Path) -> Result<libc::statfs> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", path.display())))?;
    // SAFETY: `statfs` is a plain C struct; zero initialization is a valid
    // starting state before the kernel fills it.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid C string, and `stat` points to writable memory
    // for the kernel to initialize.
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stat)
}

pub(super) fn filesystem(path: &Path) -> Result<Filesystem> {
    const BTRFS_SUPER_MAGIC: libc::c_long = 0x9123_683e;
    Ok(match statfs(path)?.f_type {
        BTRFS_SUPER_MAGIC => Filesystem::Btrfs,
        _ => Filesystem::Other,
    })
}

/// The kernel's filesystem type magic mapped to a name where one is common.
/// Unknown magics surface as a hex value rather than failing.
pub(super) fn filesystem_name(path: &Path) -> Option<String> {
    const MAGIC_NAMES: &[(libc::c_long, &str)] = &[
        (0x9123_683e, "btrfs"),
        (0x5846_5342, "xfs"),
        (0xef53, "ext"),
        (0x0102_1994, "tmpfs"),
        (0x2fc1_2fc1, "zfs"),
        (0x794c_7630, "overlayfs"),
        (0x0000_6969, "nfs"),
        (0xfe53_4d42, "smb"),
        (0xff53_4d42, "cifs"),
        (0xf2f5_2010, "f2fs"),
        (0xca45_1a4e, "bcachefs"),
        (0x2011_bab0, "exfat"),
        (0x4d44, "vfat"),
        (0x6573_5546, "fuse"),
        (0x482b, "hfs+"),
        (0x1cd1, "devpts"),
        (0x6265_6570, "configfs"),
        (0x6367_7270, "cgroup2"),
        (0x9fa0, "proc"),
        (0x6265_6572, "sysfs"),
    ];
    let stat = statfs(path).ok()?;
    Some(
        MAGIC_NAMES
            .iter()
            .find(|(magic, _)| *magic == stat.f_type)
            .map(|(_, name)| (*name).to_owned())
            .unwrap_or_else(|| format!("magic-0x{:x}", stat.f_type)),
    )
}
