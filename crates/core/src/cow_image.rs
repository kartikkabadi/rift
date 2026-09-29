//! Optional copy-on-write disk image for filesystems without one.
//!
//! On Linux, `rift init --cow-image` creates a sparse image file on the host
//! filesystem, formats it with a filesystem that supports fast clones (btrfs
//! or xfs), mounts it at a hidden directory next to the project, moves
//! the project inside, and leaves the original path working as a symlink.
//! Because rift records canonical paths, every later `rift create` lands its
//! source and destination on the image and gets instant copies.

use crate::{Error, Result};
use std::path::PathBuf;

/// What `setup` produced. `link` is the path the caller passed in, which now
/// points at `project` inside the mounted image; `backup` keeps the original
/// directory until the user deletes it.
#[derive(Debug)]
pub struct CowImage {
    pub image: PathBuf,
    pub mountpoint: PathBuf,
    pub project: PathBuf,
    pub link: PathBuf,
    pub backup: PathBuf,
    pub filesystem: String,
    pub warnings: Vec<String>,
}

/// Sets up a copy-on-write image under `project` and moves the project into
/// it. Returns `None` when `project` already lives on a filesystem that
/// supports fast clones, so no image is needed.
pub fn setup(project: &std::path::Path) -> Result<Option<CowImage>> {
    setup_impl(project)
}

#[cfg(not(target_os = "linux"))]
fn setup_impl(_: &std::path::Path) -> Result<Option<CowImage>> {
    Err(Error::CowImageSetup(
        "cow images are only supported on Linux".into(),
    ))
}

#[cfg(target_os = "linux")]
fn setup_impl(project: &std::path::Path) -> Result<Option<CowImage>> {
    let at = crate::existing_directory(project)?;
    if crate::strategy::default_strategy().probe(&at)? != crate::Backend::Portable {
        return Ok(None);
    }
    linux::setup(&at).map(Some)
}

#[cfg(target_os = "linux")]
mod linux {
    use super::CowImage;
    use crate::strategy::portable::copy_directory_portable;
    use crate::{CopyMode, Error, Result};
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Filesystems that can carry fast clones, in preference order: btrfs gets
    /// subvolume snapshots, xfs gets reflinks.
    const FILESYSTEMS: &[&str] = &["btrfs", "xfs"];

    pub(super) fn setup(at: &Path) -> Result<CowImage> {
        let name = at.file_name().ok_or_else(|| {
            Error::CowImageSetup(format!("workspace has no name: {}", at.display()))
        })?;
        let (image, mountpoint) = image_paths(at).ok_or_else(|| {
            Error::CowImageSetup(format!("cannot place an image next to {}", at.display()))
        })?;
        if image.exists() || mountpoint.exists() {
            return Err(Error::CowImageSetup(format!(
                "an image already exists at {}",
                image.display()
            )));
        }
        let filesystem = FILESYSTEMS
            .iter()
            .copied()
            .find(|fs| on_path(&format!("mkfs.{fs}")))
            .ok_or_else(|| {
                Error::CowImageSetup(
                    "no mkfs tool found; install btrfs-progs or xfsprogs".into(),
                )
            })?;
        let sudo = sudo_command()?;

        let mut outcome = match setup_image(at, name, &image, &mountpoint, filesystem, &sudo) {
            Err(error) => {
                cleanup(&image, &mountpoint, &sudo);
                return Err(error);
            }
            Ok(outcome) => outcome,
        };
        outcome
            .warnings
            .extend(record_mount(&image, &mountpoint, filesystem, &sudo).err());
        Ok(outcome)
    }

    fn setup_image(
        at: &Path,
        name: &OsStr,
        image: &Path,
        mountpoint: &Path,
        filesystem: &str,
        sudo: &[OsString],
    ) -> Result<CowImage> {
        let parent = at.parent().ok_or_else(|| {
            Error::CowImageSetup(format!("workspace has no parent: {}", at.display()))
        })?;
        std::fs::create_dir_all(mountpoint)?;
        let file = std::fs::File::create(image).map_err(|error| {
            Error::CowImageSetup(format!("cannot create {}: {error}", image.display()))
        })?;
        file.set_len(image_size_bytes(directory_bytes(at)?))?;
        drop(file);
        run(
            Command::new(format!("mkfs.{filesystem}"))
                .arg("-f")
                .arg(image),
            "format the image",
        )?;
        run(
            command(sudo, "mount")
                .arg("-o")
                .arg("loop")
                .arg("-t")
                .arg(filesystem)
                .arg(image)
                .arg(mountpoint),
            &format!("mount {}", mountpoint.display()),
        )?;
        if !sudo.is_empty() {
            let owner = format!(
                "{}:{}",
                unsafe { libc::geteuid() },
                unsafe { libc::getegid() }
            );
            run(
                command(sudo, "chown").arg(owner).arg(mountpoint),
                "hand the mount to the current user",
            )?;
        }
        let moved = mountpoint.join("workspaces").join(name);
        std::fs::create_dir_all(mountpoint.join("workspaces"))?;
        copy_directory_portable(at, &moved, CopyMode::All)?;
        let backup = parent.join(format!("{}.rift-backup", name.to_string_lossy()));
        std::fs::rename(at, &backup)?;
        if let Err(error) = std::os::unix::fs::symlink(&moved, at) {
            let _ = std::fs::rename(&backup, at);
            return Err(Error::CowImageSetup(format!(
                "could not link {} into the image: {error}",
                at.display()
            )));
        }
        Ok(CowImage {
            image: image.to_path_buf(),
            mountpoint: mountpoint.to_path_buf(),
            project: moved,
            link: at.to_path_buf(),
            backup,
            filesystem: filesystem.to_owned(),
            warnings: Vec::new(),
        })
    }

    /// The image file and mountpoint for a project, kept in a hidden sibling
    /// directory alongside rift's `.rifts` convention.
    fn image_paths(project: &Path) -> Option<(PathBuf, PathBuf)> {
        let name = project.file_name()?;
        let base = project.parent()?.join(".rifts-images");
        Some((
            base.join(format!("{}.img", name.to_string_lossy())),
            base.join(name),
        ))
    }

    /// Sparse image capacity: generous headroom for future rifts, bounded so a
    /// huge source does not reserve an impractical virtual size.
    fn image_size_bytes(source_bytes: u64) -> u64 {
        const MIN: u64 = 4 << 30;
        const MAX: u64 = 64 << 30;
        source_bytes.saturating_mul(4).clamp(MIN, MAX)
    }

    /// Whether an fstab body already references this mountpoint.
    fn fstab_has_entry(body: &str, mountpoint: &str) -> bool {
        body.lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .any(|line| line.split_whitespace().nth(1) == Some(mountpoint))
    }

    /// Adds an fstab line so the image remounts after reboot; a warning is
    /// returned rather than failing setup.
    fn record_mount(
        image: &Path,
        mountpoint: &Path,
        filesystem: &str,
        sudo: &[OsString],
    ) -> std::result::Result<(), String> {
        if mountpoint.to_string_lossy().contains(' ') || image.to_string_lossy().contains(' ') {
            return Err(format!(
                "path contains spaces; add this to /etc/fstab yourself: {} {} {} loop 0 0",
                image.display(),
                mountpoint.display(),
                filesystem
            ));
        }
        if std::fs::read_to_string("/etc/fstab")
            .is_ok_and(|body| fstab_has_entry(&body, &mountpoint.to_string_lossy()))
        {
            return Ok(());
        }
        let line = format!(
            "{} {} {} loop 0 0 # rift\n",
            image.display(),
            mountpoint.display(),
            filesystem
        );
        run(
            command(sudo, "sh")
                .arg("-c")
                .arg(format!("printf '%s' {} >> /etc/fstab", shell_quote(&line))),
            "record the mount in /etc/fstab",
        )
        .map_err(|error| {
            format!("could not update /etc/fstab ({error}); the mount is lost after reboot")
        })
    }

    /// Deletes the image artifacts after a failed setup; the project is only
    /// renamed after the mount and copy succeed, so nothing restores it.
    fn cleanup(image: &Path, mountpoint: &Path, sudo: &[OsString]) {
        let _ = run(
            command(sudo, "umount").arg("-l").arg(mountpoint),
            "unmount the image",
        );
        let _ = std::fs::remove_dir_all(mountpoint);
        let _ = std::fs::remove_file(image);
        if let Some(base) = image.parent() {
            let _ = std::fs::remove_dir(base);
        }
    }

    /// `["sudo"]` when not already root; empty when root.
    fn sudo_command() -> Result<Vec<OsString>> {
        if unsafe { libc::geteuid() } == 0 {
            return Ok(Vec::new());
        }
        if on_path("sudo") {
            return Ok(vec![OsString::from("sudo")]);
        }
        Err(Error::CowImageSetup(
            "root or `sudo` is required to mount the image".into(),
        ))
    }

    fn command(prefix: &[OsString], program: &str) -> Command {
        match prefix {
            [] => Command::new(program),
            [sudo] => {
                let mut command = Command::new(sudo);
                command.arg(program);
                command
            }
            _ => unreachable!("the sudo prefix is at most one element"),
        }
    }

    fn on_path(program: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
        })
    }

    fn directory_bytes(path: &Path) -> Result<u64> {
        let mut bytes: u64 = 0;
        for entry in walkdir::WalkDir::new(path).follow_links(false) {
            bytes = bytes.saturating_add(entry?.metadata()?.len());
        }
        Ok(bytes)
    }

    fn run(command: &mut Command, action: &str) -> Result<()> {
        let output = command
            .output()
            .map_err(|error| Error::CowImageSetup(format!("could not {action}: {error}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(Error::CowImageSetup(format!(
            "{action} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn image_paths_stay_beside_the_project() {
            let (image, mountpoint) = image_paths(Path::new("/srv/work/app")).unwrap();
            assert_eq!(image, PathBuf::from("/srv/work/.rifts-images/app.img"));
            assert_eq!(mountpoint, PathBuf::from("/srv/work/.rifts-images/app"));
        }

        #[test]
        fn image_size_scales_with_the_project_within_bounds() {
            assert_eq!(image_size_bytes(0), 4 << 30);
            assert_eq!(image_size_bytes(8 << 30), 32 << 30);
            assert_eq!(image_size_bytes(100 << 30), 64 << 30);
        }

        #[test]
        fn fstab_entry_detection_skips_comments_and_other_mounts() {
            let body =
                "UUID=x / ext4 defaults 0 1\n# /img /mnt btrfs loop 0 0\n/img /mnt2 xfs loop 0 0\n";
            assert!(!fstab_has_entry(body, "/mnt"));
            assert!(fstab_has_entry(body, "/mnt2"));
        }
    }
}
