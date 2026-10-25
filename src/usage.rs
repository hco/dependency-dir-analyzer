//! Disk-usage accounting that knows about shared storage.
//!
//! Summing file lengths overstates what deleting a directory frees whenever
//! its files share blocks with files elsewhere: hardlinks (pnpm on Linux) and
//! APFS clones (pnpm on macOS, `cp -c`). This module measures, per file, the
//! allocated bytes plus enough identity (inode, link count, clone id and clone
//! reference count) for [`UsageAccumulator`] to work out, per directory, both
//! the on-disk size and the part of it that is actually *freeable*.
//!
//! Local APFS snapshots (Time Machine) are deliberately ignored: they pin the
//! blocks of nearly every older file, so counting them would make almost
//! nothing look freeable. "Freeable" therefore means "freed once any local
//! snapshots expire".

use std::collections::HashMap;
use std::path::Path;

/// APFS clone identity of a file: every clone of the same data stream shares
/// `id`, and `refcnt` is how many files currently reference it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloneInfo {
    pub id: u64,
    pub refcnt: u32,
}

/// What [`UsageAccumulator`] needs to know about one regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    /// Bytes actually allocated on disk (`st_blocks * 512`), not the length.
    pub allocated: u64,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    /// `None` when the volume doesn't track clones (or on non-macOS).
    pub clone: Option<CloneInfo>,
}

/// The measured usage of one directory tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// On-disk size, like `du`: allocated bytes, each inode counted once.
    pub allocated: u64,
    /// The part of `allocated` that deleting the whole tree would free.
    pub freeable: u64,
}

/// Accumulates [`FileInfo`]s for one directory tree into a [`Usage`].
#[derive(Debug, Default)]
pub struct UsageAccumulator {
    dedupe_all_inodes: bool,
    usage: Usage,
    /// Multiply-referenced inodes: `(dev, ino)` → (links seen, info).
    inodes: HashMap<(u64, u64), (u64, FileInfo)>,
    /// Clone families: id → (members seen, refcnt, largest allocation).
    clones: HashMap<u64, (u32, u32, u64)>,
}

impl UsageAccumulator {
    /// `dedupe_all_inodes` makes every inode count once even when its link
    /// count is 1 — needed when following symlinks, where the same file can be
    /// reached through more than one path.
    pub fn new(dedupe_all_inodes: bool) -> Self {
        UsageAccumulator {
            dedupe_all_inodes,
            ..Default::default()
        }
    }

    pub fn add(&mut self, f: FileInfo) {
        if f.nlink > 1 || self.dedupe_all_inodes {
            // Size each inode once; whether it is freeable depends on whether
            // all of its links turn up inside this tree, known only at the end.
            let seen = self.inodes.entry((f.dev, f.ino)).or_insert((0, f));
            if seen.0 == 0 {
                self.usage.allocated += f.allocated;
            }
            seen.0 += 1;
            return;
        }
        self.usage.allocated += f.allocated;
        self.add_freeable(f);
    }

    pub fn finish(mut self) -> Usage {
        for (seen, f) in std::mem::take(&mut self.inodes).into_values() {
            if seen >= f.nlink {
                self.add_freeable(f);
            }
        }
        for (members, refcnt, allocated) in self.clones.values() {
            // A clone family's blocks are freed only once every member is gone.
            if members >= refcnt {
                self.usage.freeable += allocated;
            }
        }
        self.usage
    }

    /// Credit an inode whose links all lie inside the tree: freeable outright
    /// unless it shares blocks with clones, which `finish` settles per family.
    fn add_freeable(&mut self, f: FileInfo) {
        match f.clone {
            Some(c) if c.refcnt > 1 => {
                let family = self.clones.entry(c.id).or_insert((0, c.refcnt, 0));
                family.0 += 1;
                family.2 = family.2.max(f.allocated);
            }
            _ => self.usage.freeable += f.allocated,
        }
    }
}

/// Gather the [`FileInfo`] for the file at `path`, whose (non-followed)
/// metadata the caller already has.
#[cfg(unix)]
pub fn file_info(path: &Path, meta: &std::fs::Metadata) -> FileInfo {
    use std::os::unix::fs::MetadataExt;
    FileInfo {
        allocated: meta.blocks() * 512,
        dev: meta.dev(),
        ino: meta.ino(),
        nlink: meta.nlink(),
        clone: clone_info(path),
    }
}

/// Windows: std exposes neither block counts nor link counts on stable, so
/// every file counts as exclusive and its length stands in for its allocation.
#[cfg(not(unix))]
pub fn file_info(_path: &Path, meta: &std::fs::Metadata) -> FileInfo {
    FileInfo {
        allocated: meta.len(),
        dev: 0,
        ino: 0,
        nlink: 1,
        clone: None,
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn clone_info(_path: &Path) -> Option<CloneInfo> {
    None
}

/// Read the APFS clone id and clone reference count via `getattrlist`.
/// Returns `None` when the volume doesn't report them (non-APFS, or no full
/// clone tracking) so the file is treated as exclusive.
#[cfg(target_os = "macos")]
fn clone_info(path: &Path) -> Option<CloneInfo> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// Missing from the `libc` crate; value from `<sys/attr.h>`.
    const ATTR_CMNEXT_CLONE_REFCNT: libc::attrgroup_t = 0x0000_1000;

    // Attributes come back packed, in ascending bit order, after the length
    // and the set of attributes the volume actually returned.
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        returned: libc::attribute_set_t,
        clone_id: u64,
        refcnt: u32,
    }

    let wanted = libc::ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero bit pattern is a valid `attrlist`.
    let mut request: libc::attrlist = unsafe { std::mem::zeroed() };
    request.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    request.commonattr = libc::ATTR_CMN_RETURNED_ATTRS;
    request.forkattr = wanted;
    let mut reply = std::mem::MaybeUninit::<Reply>::zeroed();
    // SAFETY: `c_path` is NUL-terminated, `request` is fully initialised and
    // `reply` is a writable buffer of the size we pass.
    let rc = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            (&mut request as *mut libc::attrlist).cast(),
            reply.as_mut_ptr().cast(),
            std::mem::size_of::<Reply>(),
            libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: the buffer was zero-initialised and the call succeeded.
    let reply = unsafe { reply.assume_init() };
    let returned = reply.returned.forkattr;
    if returned & wanted != wanted {
        return None;
    }
    Some(CloneInfo {
        id: reply.clone_id,
        refcnt: reply.refcnt,
    })
}

/// Whether the boot volume currently holds local Time Machine snapshots, which
/// keep deleted files' blocks allocated until they expire. Always `false`
/// off macOS or if `tmutil` can't be run.
pub fn has_local_snapshots() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("tmutil")
        .args(["listlocalsnapshotdates", "/"])
        .output()
        .map(|out| lists_snapshots(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or(false)
}

/// `tmutil listlocalsnapshotdates` prints a header line, then one date per
/// snapshot.
fn lists_snapshots(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(allocated: u64, ino: u64, nlink: u64, clone: Option<(u64, u32)>) -> FileInfo {
        FileInfo {
            allocated,
            dev: 1,
            ino,
            nlink,
            clone: clone.map(|(id, refcnt)| CloneInfo { id, refcnt }),
        }
    }

    fn usage(dedupe: bool, files: &[FileInfo]) -> Usage {
        let mut acc = UsageAccumulator::new(dedupe);
        for file in files {
            acc.add(*file);
        }
        acc.finish()
    }

    fn u(allocated: u64, freeable: u64) -> Usage {
        Usage {
            allocated,
            freeable,
        }
    }

    #[test]
    fn exclusive_files_are_fully_freeable() {
        let files = [f(4096, 1, 1, None), f(4096, 2, 1, None)];
        assert_eq!(usage(false, &files), u(8192, 8192));
    }

    #[test]
    fn hardlink_inside_dir_counts_once_and_is_freeable() {
        let files = [f(4096, 7, 2, None), f(4096, 7, 2, None)];
        assert_eq!(usage(false, &files), u(4096, 4096));
    }

    #[test]
    fn hardlink_to_outside_is_not_freeable() {
        assert_eq!(usage(false, &[f(4096, 7, 2, None)]), u(4096, 0));
    }

    #[test]
    fn clone_shared_with_outside_is_not_freeable() {
        assert_eq!(usage(false, &[f(4096, 1, 1, Some((99, 2)))]), u(4096, 0));
    }

    #[test]
    fn clone_family_fully_inside_counts_once() {
        let files = [f(4096, 1, 1, Some((99, 2))), f(4096, 2, 1, Some((99, 2)))];
        assert_eq!(usage(false, &files), u(8192, 4096));
    }

    #[test]
    fn missing_clone_info_is_exclusive() {
        assert_eq!(usage(false, &[f(8192, 3, 1, None)]), u(8192, 8192));
        // A clone whose siblings are all gone is exclusive again.
        assert_eq!(usage(false, &[f(8192, 3, 1, Some((5, 1)))]), u(8192, 8192));
    }

    #[test]
    fn snapshot_listing_is_recognised() {
        let some = "Snapshot dates for all disks:\n2026-10-25-083241\n2026-10-25-093319\n";
        assert!(lists_snapshots(some));
        assert!(!lists_snapshots("Snapshot dates for all disks:\n"));
        assert!(!lists_snapshots(""));
    }

    #[test]
    fn follow_symlinks_counts_inode_once() {
        let files = [f(4096, 5, 1, None), f(4096, 5, 1, None)];
        assert_eq!(usage(true, &files), u(4096, 4096));
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dda-usage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn info(path: &Path) -> FileInfo {
        let meta = std::fs::symlink_metadata(path).unwrap();
        file_info(path, &meta)
    }

    #[cfg(unix)]
    #[test]
    fn file_info_reads_real_hardlinks() {
        let dir = scratch("hardlink");
        let a = dir.join("a");
        std::fs::write(&a, vec![7u8; 64 * 1024]).unwrap();
        std::fs::hard_link(&a, dir.join("b")).unwrap();

        let (ia, ib) = (info(&a), info(&dir.join("b")));
        assert_eq!(ia.nlink, 2);
        assert_eq!(ia.ino, ib.ino);
        assert!(ia.allocated >= 64 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn file_info_reads_apfs_clones() {
        let dir = scratch("clone");
        let a = dir.join("a");
        std::fs::write(&a, vec![7u8; 64 * 1024]).unwrap();
        let status = std::process::Command::new("cp")
            .arg("-c")
            .arg(&a)
            .arg(dir.join("b"))
            .status()
            .unwrap();
        assert!(status.success());

        // The temp dir lives on the APFS data volume on any current macOS.
        let (ia, ib) = (info(&a), info(&dir.join("b")));
        let (ca, cb) = (ia.clone.unwrap(), ib.clone.unwrap());
        assert_eq!(ca.id, cb.id);
        assert_eq!(ca.refcnt, 2);
        assert_ne!(ia.ino, ib.ino);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
