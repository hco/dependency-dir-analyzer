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
        let mut rest = bytes;
        for _ in 0..count {
            let Some(len) = read_u32(rest, 0).map(|len| len as usize) else {
                return;
            };
            // A short or overlong length would loop forever or read past the
            // buffer; neither should happen, so give up on the directory.
            if len < offset::COMMON_END || len > rest.len() {
                return;
            }
            entry(dir, &rest[..len], stack, visit);
            rest = &rest[len..];
        }
    }
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
