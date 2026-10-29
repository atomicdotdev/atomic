//! Never resolve a deletion through ambient path parents after preflight.
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use anyhow::{ensure, Context};
use rustix::fd::OwnedFd;
use rustix::fs::{fstat, open, openat, statat, unlinkat, AtFlags, Dir, FileType, Mode, OFlags};

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) fn remove(root: &Path, path: &Path, expected: &std::fs::Metadata) -> anyhow::Result<()> {
    let relative = path
        .strip_prefix(root)
        .context("cleanup outside metadata root")?;
    let name = relative
        .file_name()
        .context("refusing to delete the metadata root")?;
    let mut parent = open(root, DIRECTORY, Mode::empty())?;
    let device = fstat(&parent)?.st_dev;
    for component in relative
        .parent()
        .context("missing relative parent")?
        .components()
    {
        let Component::Normal(name) = component else {
            anyhow::bail!("invalid cleanup component");
        };
        parent = openat(&parent, name, DIRECTORY, Mode::empty())?;
        ensure!(
            fstat(&parent)?.st_dev == device,
            "cleanup refuses a filesystem mount boundary"
        );
    }
    let directory = openat(&parent, name, DIRECTORY, Mode::empty())?;
    let observed = fstat(&directory)?;
    ensure!(
        observed.st_dev == device,
        "cleanup refuses a mounted candidate"
    );
    ensure!(
        observed.st_dev as u64 == expected.dev() && observed.st_ino as u64 == expected.ino(),
        "cleanup root was replaced after preflight"
    );
    remove_directory(&parent, name, directory, 0)
}

fn remove_directory(
    parent: &OwnedFd,
    name: &OsStr,
    directory: OwnedFd,
    depth: usize,
) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(depth < 512, "cleanup tree is too deep");
    let original = fstat(&directory)?;
    for entry in Dir::read_from(&directory)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let child = OsStr::from_bytes(bytes);
        let stat = statat(&directory, child, AtFlags::SYMLINK_NOFOLLOW)?;
        ensure!(
            stat.st_dev == original.st_dev,
            "cleanup refuses a nested filesystem mount boundary"
        );
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                let child_fd = openat(&directory, child, DIRECTORY, Mode::empty())?;
                let opened = fstat(&child_fd)?;
                ensure!(
                    stat.st_dev == opened.st_dev && stat.st_ino == opened.st_ino,
                    "cleanup child directory was replaced"
                );
                remove_directory(&directory, child, child_fd, depth + 1)?;
            }
            FileType::RegularFile | FileType::Symlink => {
                unlinkat(&directory, child, AtFlags::empty())?
            }
            _ => anyhow::bail!("cleanup refuses a special file"),
        }
    }
    let current = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    ensure!(
        current.st_dev == original.st_dev && current.st_ino == original.st_ino,
        "cleanup directory name was replaced"
    );
    unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    Ok(())
}
