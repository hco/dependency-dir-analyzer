//! macOS directory walk via `getattrlistbulk`.
//!
//! Sizing a tree file by file costs an `lstat` plus a `getattrlist` (for the
//! APFS clone id) per file. `getattrlistbulk` returns all of that for a whole
//! directory's entries in a few calls, which makes sizing large
//! `node_modules` trees several times faster.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::usage::{CloneInfo, FileInfo};

/// What [`walk`] reports for each entry beneath the root.
pub enum Visit<'a> {
    /// A regular file.
    File(FileInfo),
    /// The modification time of any entry (file, directory, symlink, …).
    Modified(SystemTime),
    /// A directory whose volume doesn't support `getattrlistbulk`; nothing
    /// beneath it has been reported, so the caller should walk it another way.
    Unsupported(&'a Path),
}

/// Missing from the `libc` crate; values from `<sys/attr.h>` and
/// `<sys/vnode.h>`.
const ATTR_CMNEXT_CLONE_REFCNT: libc::attrgroup_t = 0x0000_1000;
const VREG: u32 = 1;
const VDIR: u32 = 2;

const COMMON: libc::attrgroup_t = libc::ATTR_CMN_RETURNED_ATTRS
    | libc::ATTR_CMN_NAME
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_FILEID;
const FILE: libc::attrgroup_t = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE;
const CLONE: libc::attrgroup_t = libc::ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT;

/// With `FSOPT_PACK_INVAL_ATTRS` every requested attribute that applies to
/// the entry's type is packed, valid or not, so each lies at a fixed offset
/// from the start of its entry: the length, the returned `attribute_set_t`,
/// then the attributes in bit order. File attributes are only packed for
/// files, so non-file entries end after the common ones.
mod offset {
    pub const RETURNED_COMMON: usize = 4;
    pub const RETURNED_FILE: usize = 16;
    pub const RETURNED_FORK: usize = 20;
    pub const NAME: usize = 24;
    pub const DEV: usize = 32;
    pub const OBJTYPE: usize = 36;
    pub const MTIME_SEC: usize = 40;
    pub const MTIME_NSEC: usize = 48;
    pub const INO: usize = 56;
    pub const NLINK: usize = 64;
    pub const ALLOCATED: usize = 68;
    pub const CLONE_ID: usize = 76;
    pub const CLONE_REFCNT: usize = 84;
    /// Where the fixed part of any entry ends.
    pub const COMMON_END: usize = 64;
    /// Where the fixed part of a regular file's entry ends.
    pub const FILE_END: usize = 88;
}

/// Walk everything beneath `root` (not `root` itself), reporting to `visit`.
/// Unreadable directories are skipped silently, like the walkdir path does.
pub fn walk(root: &Path, cancel: &AtomicBool, visit: &mut dyn FnMut(Visit<'_>)) {
    // u64s so the buffer is as aligned as the kernel could want.
    let mut buf = vec![0u64; 8 * 1024];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        read_dir(&dir, &mut buf, cancel, &mut stack, visit);
    }
}

/// Report every entry of `dir`, pushing its subdirectories onto `stack`.
fn read_dir(
    dir: &Path,
    buf: &mut [u64],
    cancel: &AtomicBool,
    stack: &mut Vec<PathBuf>,
    visit: &mut dyn FnMut(Visit<'_>),
) {
    let Ok(handle) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)
    else {
        return;
    };
    // SAFETY: an all-zero bit pattern is a valid `attrlist`.
    let mut request: libc::attrlist = unsafe { std::mem::zeroed() };
    request.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    request.commonattr = COMMON;
    request.fileattr = FILE;
    request.forkattr = CLONE;
    let buf_len = std::mem::size_of_val(buf);

    let mut first = true;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        // SAFETY: `handle` is an open directory, `request` is fully
        // initialised and `buf` is writable for `buf_len` bytes.
        let count = unsafe {
            libc::getattrlistbulk(
                handle.as_raw_fd(),
                (&mut request as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                buf_len,
                u64::from(libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_PACK_INVAL_ATTRS),
            )
        };
        if count < 0 && first {
            // Nothing of `dir` has been reported yet, so it can be handed
            // back whole.
            visit(Visit::Unsupported(dir));
        }
        if count <= 0 {
            return;
        }
        first = false;
        // SAFETY: `buf` is `buf_len` initialised bytes, and u8 has no
        // alignment or validity requirements.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf_len) };
        if !batch(dir, bytes, count as usize, stack, visit) {
            return;
        }
    }
}

/// Report the `count` packed entries of `dir` at the start of `bytes`.
/// Returns `false` if the batch is malformed and the directory should be
/// given up on.
fn batch(
    dir: &Path,
    bytes: &[u8],
    count: usize,
    stack: &mut Vec<PathBuf>,
    visit: &mut dyn FnMut(Visit<'_>),
) -> bool {
    let mut rest = bytes;
    for _ in 0..count {
        let Some(len) = read_u32(rest, 0).map(|len| len as usize) else {
            return false;
        };
        // A short or overlong length would loop forever or read past the
        // buffer; neither should happen, so give up on the directory.
        if len < offset::COMMON_END || len > rest.len() {
            return false;
        }
        entry(dir, &rest[..len], stack, visit);
        rest = &rest[len..];
    }
    true
}

/// Report one packed entry of `dir`.
fn entry(dir: &Path, e: &[u8], stack: &mut Vec<PathBuf>, visit: &mut dyn FnMut(Visit<'_>)) {
    let returned = |at| read_u32(e, at).unwrap_or(0);
    let has = |at, bits| returned(at) & bits == bits;
    if !has(
        offset::RETURNED_COMMON,
        libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE,
    ) {
        return;
    }
    if has(offset::RETURNED_COMMON, libc::ATTR_CMN_MODTIME) {
        if let Some(modified) = mtime(e) {
            visit(Visit::Modified(modified));
        }
    }
    match read_u32(e, offset::OBJTYPE) {
        Some(VDIR) => {
            if let Some(name) = name(e) {
                stack.push(dir.join(name));
            }
        }
        Some(VREG) => {
            let complete = has(
                offset::RETURNED_COMMON,
                libc::ATTR_CMN_DEVID | libc::ATTR_CMN_FILEID,
            ) && has(offset::RETURNED_FILE, FILE)
                && e.len() >= offset::FILE_END;
            if complete {
                visit(Visit::File(file_info(e)));
            } else if let Some(name) = name(e) {
                // Rare enough that the per-file calls don't matter.
                let path = dir.join(name);
                if let Ok(meta) = std::fs::symlink_metadata(&path) {
                    visit(Visit::File(crate::usage::file_info(&path, &meta)));
                }
            }
        }
        _ => {}
    }
}

fn file_info(e: &[u8]) -> FileInfo {
    let clone = (read_u32(e, offset::RETURNED_FORK).unwrap_or(0) & CLONE == CLONE)
        .then(|| {
            Some(CloneInfo {
                id: read_u64(e, offset::CLONE_ID)?,
                refcnt: read_u32(e, offset::CLONE_REFCNT)?,
            })
        })
        .flatten();
    FileInfo {
        allocated: read_u64(e, offset::ALLOCATED).unwrap_or(0),
        // `dev_t` is an i32; widen it the way std's `MetadataExt::dev` does.
        id: Some((
            read_u32(e, offset::DEV).unwrap_or(0) as i32 as u64,
            read_u64(e, offset::INO).unwrap_or(0),
        )),
        nlink: u64::from(read_u32(e, offset::NLINK).unwrap_or(1)),
        clone,
    }
}

/// The entry's name, from the `attrreference_t` that points at it relative to
/// its own position.
fn name(e: &[u8]) -> Option<&OsStr> {
    let start =
        offset::NAME.checked_add(usize::try_from(read_u32(e, offset::NAME)? as i32).ok()?)?;
    let len = read_u32(e, offset::NAME + 4)? as usize;
    let bytes = e.get(start..start.checked_add(len)?)?;
    // The length includes the terminating NUL.
    let bytes = bytes.split(|&b| b == 0).next()?;
    (!bytes.is_empty()).then(|| OsStr::from_bytes(bytes))
}

fn mtime(e: &[u8]) -> Option<SystemTime> {
    let secs = read_u64(e, offset::MTIME_SEC)? as i64;
    let nanos = read_u64(e, offset::MTIME_NSEC)?;
    let nanos = Duration::from_nanos(nanos);
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64) + nanos)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(secs.unsigned_abs()))?
            .checked_add(nanos)
    }
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_COMMON: u32 = COMMON;

    /// The fields of one synthetic `getattrlistbulk` entry.
    #[derive(Clone)]
    struct Packed {
        kind: u32,
        name: &'static str,
        returned: (u32, u32, u32),
        mtime: (i64, u64),
        dev: i32,
        ino: u64,
        nlink: u32,
        allocated: u64,
        clone: (u64, u32),
    }

    fn file(name: &'static str) -> Packed {
        Packed {
            kind: VREG,
            name,
            returned: (ALL_COMMON, FILE, CLONE),
            mtime: (1_700_000_000, 5),
            dev: 0x0100_0010,
            ino: 42,
            nlink: 2,
            allocated: 8192,
            clone: (7, 3),
        }
    }

    fn dir(name: &'static str) -> Packed {
        Packed {
            kind: VDIR,
            returned: (ALL_COMMON, 0, 0),
            ..file(name)
        }
    }

    /// Lay `p` out the way the kernel packs it, name after the fixed part.
    fn pack(p: &Packed) -> Vec<u8> {
        let fixed = if p.kind == VREG {
            offset::FILE_END
        } else {
            offset::COMMON_END
        };
        let mut e = vec![0u8; fixed];
        let put = |e: &mut Vec<u8>, at: usize, bytes: &[u8]| {
            e[at..at + bytes.len()].copy_from_slice(bytes)
        };
        put(&mut e, offset::RETURNED_COMMON, &p.returned.0.to_ne_bytes());
        put(&mut e, offset::RETURNED_FILE, &p.returned.1.to_ne_bytes());
        put(&mut e, offset::RETURNED_FORK, &p.returned.2.to_ne_bytes());
        put(
            &mut e,
            offset::NAME,
            &((fixed - offset::NAME) as i32).to_ne_bytes(),
        );
        put(
            &mut e,
            offset::NAME + 4,
            &(p.name.len() as u32 + 1).to_ne_bytes(),
        );
        put(&mut e, offset::DEV, &p.dev.to_ne_bytes());
        put(&mut e, offset::OBJTYPE, &p.kind.to_ne_bytes());
        put(&mut e, offset::MTIME_SEC, &p.mtime.0.to_ne_bytes());
        put(&mut e, offset::MTIME_NSEC, &p.mtime.1.to_ne_bytes());
        put(&mut e, offset::INO, &p.ino.to_ne_bytes());
        if p.kind == VREG {
            put(&mut e, offset::NLINK, &p.nlink.to_ne_bytes());
            put(&mut e, offset::ALLOCATED, &p.allocated.to_ne_bytes());
            put(&mut e, offset::CLONE_ID, &p.clone.0.to_ne_bytes());
            put(&mut e, offset::CLONE_REFCNT, &p.clone.1.to_ne_bytes());
        }
        e.extend_from_slice(p.name.as_bytes());
        e.push(0);
        while e.len() % 4 != 0 {
            e.push(0);
        }
        let len = e.len() as u32;
        put(&mut e, 0, &len.to_ne_bytes());
        e
    }

    #[derive(Default)]
    struct Seen {
        complete: bool,
        files: Vec<FileInfo>,
        modified: Vec<SystemTime>,
        queued: Vec<PathBuf>,
    }

    /// Parse `entries` as one batch of `dir`.
    fn parse(dir: &Path, entries: &[Vec<u8>]) -> Seen {
        let bytes = entries.concat();
        let mut seen = Seen::default();
        let mut files = Vec::new();
        let mut modified = Vec::new();
        seen.complete = batch(
            dir,
            &bytes,
            entries.len(),
            &mut seen.queued,
            &mut |v| match v {
                Visit::File(f) => files.push(f),
                Visit::Modified(m) => modified.push(m),
                Visit::Unsupported(_) => panic!("a parsed batch is never unsupported"),
            },
        );
        seen.files = files;
        seen.modified = modified;
        seen
    }

    #[test]
    fn file_entries_are_read_from_their_packed_fields() {
        let seen = parse(Path::new("/d"), &[pack(&file("a.js"))]);
        assert_eq!(
            seen.files,
            [FileInfo {
                allocated: 8192,
                id: Some((0x0100_0010, 42)),
                nlink: 2,
                clone: Some(CloneInfo { id: 7, refcnt: 3 }),
            }]
        );
        assert_eq!(
            seen.modified,
            [UNIX_EPOCH + Duration::new(1_700_000_000, 5)]
        );
        assert!(seen.queued.is_empty());
    }

    #[test]
    fn clone_fields_count_only_when_returned() {
        let mut p = file("a.js");
        p.returned.2 = libc::ATTR_CMNEXT_CLONEID;
        assert_eq!(parse(Path::new("/d"), &[pack(&p)]).files[0].clone, None);
    }

    #[test]
    fn incomplete_file_entries_fall_back_to_lstat() {
        let tmp = std::env::temp_dir().join(format!("dda-bulk-{}-lstat", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("real"), vec![1u8; 10_000]).unwrap();
        let mut p = file("real");
        p.returned.1 = 0;
        let seen = parse(&tmp, &[pack(&p)]);
        let meta = std::fs::symlink_metadata(tmp.join("real")).unwrap();
        assert_eq!(
            seen.files,
            [crate::usage::file_info(&tmp.join("real"), &meta)]
        );
        // Gone by the time it is looked at: skipped, like walkdir would.
        let mut gone = file("gone");
        gone.returned.1 = 0;
        assert!(parse(&tmp, &[pack(&gone)]).files.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn directories_are_queued_and_other_types_only_dated() {
        let mut link = dir("link");
        link.kind = 5; // VLNK
        let seen = parse(Path::new("/d"), &[pack(&dir("sub")), pack(&link)]);
        assert_eq!(seen.queued, [PathBuf::from("/d/sub")]);
        assert!(seen.files.is_empty());
        assert_eq!(seen.modified.len(), 2);
    }

    #[test]
    fn names_pointing_outside_the_entry_are_ignored() {
        let mut e = pack(&dir("sub"));
        e[offset::NAME..offset::NAME + 4].copy_from_slice(&1000i32.to_ne_bytes());
        assert!(parse(Path::new("/d"), &[e.clone()]).queued.is_empty());
        e[offset::NAME..offset::NAME + 4].copy_from_slice(&(-8i32).to_ne_bytes());
        assert!(parse(Path::new("/d"), &[e]).queued.is_empty());
    }

    #[test]
    fn entries_without_name_or_type_are_skipped() {
        let mut p = dir("sub");
        p.returned.0 = ALL_COMMON & !libc::ATTR_CMN_OBJTYPE;
        let seen = parse(Path::new("/d"), &[pack(&p)]);
        assert!(seen.queued.is_empty());
        assert!(seen.modified.is_empty());
    }

    #[test]
    fn modification_times_before_the_epoch_are_kept() {
        let mut p = file("old");
        p.mtime = (-10, 500);
        assert_eq!(
            parse(Path::new("/d"), &[pack(&p)]).modified,
            [UNIX_EPOCH - Duration::from_secs(10) + Duration::from_nanos(500)]
        );
    }

    #[test]
    fn malformed_lengths_end_the_batch() {
        let good = pack(&file("a"));
        // A zero length would never advance; an overlong one reads past the
        // buffer. Both stop the batch after what was parsed before them.
        let mut zero = pack(&file("b"));
        zero[0..4].copy_from_slice(&0u32.to_ne_bytes());
        let mut long = pack(&file("c"));
        long[0..4].copy_from_slice(&10_000u32.to_ne_bytes());
        for bad in [zero, long] {
            let seen = parse(Path::new("/d"), &[good.clone(), bad, good.clone()]);
            assert_eq!(seen.files.len(), 1);
            assert!(!seen.complete);
        }
        assert!(parse(Path::new("/d"), &[good.clone(), good]).complete);
    }

    #[test]
    fn file_entries_too_short_for_file_fields_fall_back() {
        // Packed like a directory, so the file fields would be the name.
        let mut e = pack(&dir("missing"));
        e[offset::OBJTYPE..offset::OBJTYPE + 4].copy_from_slice(&VREG.to_ne_bytes());
        e[offset::RETURNED_FILE..offset::RETURNED_FILE + 4].copy_from_slice(&FILE.to_ne_bytes());
        assert!(e.len() < offset::FILE_END);
        assert!(parse(Path::new("/nonexistent"), &[e]).files.is_empty());
    }
}
