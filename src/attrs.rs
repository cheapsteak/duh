//! macOS `getattrlistbulk` / `getattrlist` FFI with APFS clone-ID detection.
//!
//! Ports the proven ctypes binding from the Python oracle (`./duh:47-115`) and
//! extends it with a bulk directory reader for the scanner (Task 9).
//!
//! Constants are verified against the current macOS SDK headers:
//!   /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/sys/{attr,vnode}.h
//! (header line numbers noted inline). `ATTR_CMNEXT_CLONEID = 0x100` matches both
//! the header (attr.h:558) and the oracle's empirical note (`./duh:49`).

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Attributes for a single directory entry (or scan root).
///
/// `size_blocks` is allocated bytes with `st_blocks * 512` semantics everywhere
/// (Python parity). For regular files/symlinks it comes from `ATTR_FILE_ALLOCSIZE`,
/// which was empirically verified equal to `st_blocks * 512` on APFS, including
/// for decmpfs-compressed files (see comment in `parse_record`); directories and
/// `stat_root` derive it from lstat's `st_blocks * 512` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryAttrs {
    pub name: OsString,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub dev: i32,
    pub ino: u64,
    pub nlink: u32,
    pub size_logical: u64,
    pub size_blocks: u64,
    pub mtime: i64,
    pub clone_id: Option<u64>,
}

// --- attr.h constants (verified against the SDK header; lines noted) ---------
const ATTR_BIT_MAP_COUNT: u16 = 5; // attr.h:88
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000; // attr.h:449
const ATTR_CMN_NAME: u32 = 0x0000_0001; // attr.h:402
const ATTR_CMN_DEVID: u32 = 0x0000_0002; // attr.h:403
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008; // attr.h:405
const ATTR_CMN_MODTIME: u32 = 0x0000_0400; // attr.h:412
const ATTR_CMN_FILEID: u32 = 0x0200_0000; // attr.h:438
const ATTR_CMN_ERROR: u32 = 0x2000_0000; // attr.h:442

const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001; // attr.h:532
const ATTR_FILE_TOTALSIZE: u32 = 0x0000_0002; // attr.h:533
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004; // attr.h:534

const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100; // attr.h:558 (== oracle empirical, ./duh:49)

const FSOPT_NOFOLLOW: u32 = 0x0000_0001; // attr.h:46
const FSOPT_PACK_INVAL_ATTRS: u32 = 0x0000_0008; // attr.h:50
const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020; // attr.h:53

// --- vnode.h fsobj_type_t (VNON=0, VREG=1, VDIR=2, VBLK, VCHR, VLNK=5) --------
const VREG: u32 = 1; // vnode.h:85
const VDIR: u32 = 2; // vnode.h:85
const VLNK: u32 = 5; // vnode.h:85

/// The attribute set requested for bulk directory reads. Every one of these is
/// packed into the output buffer at a fixed position thanks to
/// `FSOPT_PACK_INVAL_ATTRS`; the returned `attribute_set_t` tells us which are valid.
fn bulk_attrlist() -> libc::attrlist {
    libc::attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_DEVID
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_MODTIME
            | ATTR_CMN_FILEID
            | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE,
        forkattr: ATTR_CMNEXT_CLONEID, // extended common attr, lives in forkattr field
    }
}

// Unaligned little-endian reads: getattrlistbulk packs fields tightly, so the
// buffer is not guaranteed to be naturally aligned for u64 reads.
fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn i32_at(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn u64_at(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}
fn i64_at(buf: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

const OFF_RETURNED: usize = 4; // attribute_set_t follows the u32 length

/// Parse one bulk record. The set of fields actually present is driven by the
/// returned `attribute_set_t` — NOT fixed: e.g. directories omit the `ATTR_FILE_*`
/// fields entirely (verified via DUH_ATTR_DEBUG dumps). We therefore walk a cursor
/// in canonical order, consuming a field only if its returned bit is set. Fields
/// are packed *tightly* (no alignment padding, so unaligned reads are required).
/// `ATTR_CMN_ERROR`, when requested, is packed immediately after the returned set
/// (ahead of bit order), per getattrlist(2).
///
/// Canonical order: RETURNED_ATTRS (the set itself) | ERROR | then common attrs in
/// bit order (NAME, DEVID, OBJTYPE, MODTIME, FILEID) | file attrs (LINKCOUNT,
/// TOTALSIZE, ALLOCSIZE) | forkattr (CLONEID).
fn parse_record(rec: &[u8], dir: &Path) -> Option<EntryAttrs> {
    // Caller (read_dir_attrs) guarantees rec.len() >= 24, so the returned-set
    // header reads below are in bounds; keep a defensive check regardless.
    if rec.len() < 24 {
        return None;
    }
    // returned attribute_set_t: commonattr, volattr, dirattr, fileattr, forkattr
    let ret_common = u32_at(rec, OFF_RETURNED);
    let ret_file = u32_at(rec, OFF_RETURNED + 12);
    let ret_fork = u32_at(rec, OFF_RETURNED + 16);

    let mut cur = OFF_RETURNED + 20; // first attribute after the returned set

    macro_rules! take {
        ($n:expr) => {{
            let at = cur;
            cur += $n;
            if cur > rec.len() {
                return None;
            }
            at
        }};
    }

    // ATTR_CMN_ERROR is packed first (before NAME) when requested.
    if (ret_common & ATTR_CMN_ERROR) != 0 {
        let at = take!(4);
        let err = u32_at(rec, at);
        if err != 0 {
            eprintln!("duh: skipping entry with getattrlistbulk error {err}");
            return None;
        }
    }

    // ATTR_CMN_NAME: attrreference {i32 offset (relative to itself), u32 len}.
    let name = if (ret_common & ATTR_CMN_NAME) != 0 {
        let at = take!(8);
        let name_off = i32_at(rec, at);
        // The name region always follows the attrreference; a negative offset is
        // malformed (and would overflow the unsigned arithmetic below).
        if name_off < 0 {
            return None;
        }
        let name_len = u32_at(rec, at + 4) as usize; // includes trailing NUL
        let start = at + name_off as usize;
        let end = start.checked_add(name_len.saturating_sub(1))?;
        if end > rec.len() {
            return None;
        }
        OsString::from_vec(rec[start..end].to_vec())
    } else {
        return None;
    };

    let dev = if (ret_common & ATTR_CMN_DEVID) != 0 {
        i32_at(rec, take!(4))
    } else {
        0
    };
    let objtype = if (ret_common & ATTR_CMN_OBJTYPE) != 0 {
        u32_at(rec, take!(4))
    } else {
        0
    };
    let mtime = if (ret_common & ATTR_CMN_MODTIME) != 0 {
        i64_at(rec, take!(16)) // timespec: read tv_sec (first i64)
    } else {
        0
    };
    let ino = if (ret_common & ATTR_CMN_FILEID) != 0 {
        u64_at(rec, take!(8))
    } else {
        0
    };

    // File attrs: present (with valid bits) for files and symlinks; absent for
    // directories. Fall back to lstat when any are missing (Python parity — the
    // dir entry's own st_size / st_blocks).
    //
    // size_blocks semantics: st_blocks*512 everywhere. ATTR_FILE_ALLOCSIZE was
    // verified to equal st_blocks*512 on APFS INCLUDING decmpfs-compressed files
    // (ditto --hfsCompression, 4 MiB logical text file: ALLOCSIZE=36864 ==
    // st_blocks(72)*512=36864; uncompressed twin: 4194304 == 8192*512), so the
    // bulk value and the lstat fallback below are the same derivation.
    let want_file = ATTR_FILE_LINKCOUNT | ATTR_FILE_TOTALSIZE | ATTR_FILE_ALLOCSIZE;
    let (nlink, size_logical, size_blocks) = if (ret_file & want_file) == want_file {
        let nlink = u32_at(rec, take!(4));
        let logical = i64_at(rec, take!(8)) as u64;
        let blocks = i64_at(rec, take!(8)) as u64;
        (nlink, logical, blocks)
    } else {
        lstat_sizes(&dir.join(&name)).unwrap_or((1, 0, 0))
    };

    let is_dir = objtype == VDIR;
    let is_symlink = objtype == VLNK;

    // Clone id: the kernel returns a forkattr CLONEID for every object (a dir's is
    // just its own id), but the Python oracle only records clone_ids for regular
    // files (./duh:463-468) — match that parity so clone-family logic (Task 9)
    // sees exactly what the reference does. Still consume the field to keep the
    // cursor aligned when present.
    let raw_clone = if (ret_fork & ATTR_CMNEXT_CLONEID) != 0 {
        let cid = u64_at(rec, take!(8));
        (cid != 0).then_some(cid)
    } else {
        None
    };
    let clone_id = if objtype == VREG { raw_clone } else { None };

    Some(EntryAttrs {
        name,
        is_dir,
        is_symlink,
        dev,
        ino,
        nlink,
        size_logical,
        size_blocks,
        mtime,
        clone_id,
    })
}

/// (nlink, size_logical, size_blocks) from a per-path `lstat`.
fn lstat_sizes(path: &Path) -> Option<(u32, u64, u64)> {
    let md = std::fs::symlink_metadata(path).ok()?;
    Some((
        md.nlink() as u32,
        md.size(),
        md.blocks() * 512,
    ))
}

/// Bulk-read directory entries via `getattrlistbulk`. Does not include `.`/`..`.
pub fn read_dir_attrs(dir: &Path) -> std::io::Result<Vec<EntryAttrs>> {
    let f = std::fs::File::open(dir)?; // O_RDONLY dirfd
    let attrs = bulk_attrlist();
    let options = (FSOPT_NOFOLLOW | FSOPT_PACK_INVAL_ATTRS | FSOPT_ATTR_CMN_EXTENDED) as u64;
    let mut buf = vec![0u8; 256 * 1024];
    let mut out = Vec::new();
    let debug = std::env::var_os("DUH_ATTR_DEBUG").is_some();
    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                f.as_raw_fd(),
                &attrs as *const _ as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                options,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n == 0 {
            break;
        }
        let mut off = 0usize;
        for _ in 0..n {
            // Harden against malformed/truncated records (e.g. a corrupt length
            // from a racing filesystem): every record must hold at least its u32
            // length + the 20-byte returned attribute_set_t, and must fit within
            // the buffer. On violation, abandon the rest of this batch and keep
            // what we have — a skipped entry beats a crashed scan.
            if off + 24 > buf.len() {
                eprintln!("duh: truncated getattrlistbulk record header; skipping rest of batch");
                break;
            }
            let rec_len = u32_at(&buf, off) as usize;
            if rec_len < 24 || off + rec_len > buf.len() {
                eprintln!(
                    "duh: malformed getattrlistbulk record (len={rec_len} at offset {off}); skipping rest of batch"
                );
                break;
            }
            let rec = &buf[off..off + rec_len];
            if debug {
                eprintln!("duh-debug: rec_len={rec_len} bytes={:02x?}", &rec[..rec_len.min(112)]);
            }
            if let Some(e) = parse_record(rec, dir) {
                out.push(e);
            }
            off += rec_len;
        }
    }
    Ok(out)
}

/// Return the APFS clone ID for `path`, or `None` if unavailable/unsupported.
/// Direct port of the oracle's single-path `getattrlist` binding (`./duh:74-115`).
pub fn get_clone_id(path: &Path) -> Option<u64> {
    let cpath = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;

    // attrlist requesting only ATTR_CMN_RETURNED_ATTRS + ATTR_CMNEXT_CLONEID.
    let attrs = libc::attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: ATTR_CMNEXT_CLONEID,
    };
    let mut out = [0u8; 64];
    let options = FSOPT_NOFOLLOW | FSOPT_PACK_INVAL_ATTRS | FSOPT_ATTR_CMN_EXTENDED;

    let ret = unsafe {
        libc::getattrlist(
            cpath.as_ptr(),
            &attrs as *const _ as *mut libc::c_void,
            out.as_mut_ptr() as *mut libc::c_void,
            out.len(),
            options,
        )
    };
    if ret != 0 {
        return None;
    }

    // Layout: total_len u32 @0; attribute_set_t returned @4..24 (forkattr @20);
    // clone_id u64 @24.
    let total_len = u32_at(&out, 0);
    if total_len < 32 {
        return None;
    }
    let returned_forkattr = u32_at(&out, 20);
    if (returned_forkattr & ATTR_CMNEXT_CLONEID) == 0 {
        return None;
    }
    let clone_id = u64_at(&out, 24);
    if clone_id == 0 {
        None
    } else {
        Some(clone_id)
    }
}

/// Stat a single path (typically a scan root) into an [`EntryAttrs`], following
/// symlinks is NOT done — the root's own metadata is returned (lstat semantics).
pub fn stat_root(path: &Path) -> std::io::Result<EntryAttrs> {
    let md = std::fs::symlink_metadata(path)?;
    let ft = md.file_type();
    let name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| path.as_os_str().to_os_string());
    let clone_id = if ft.is_file() {
        get_clone_id(path)
    } else {
        None
    };
    Ok(EntryAttrs {
        name,
        is_dir: ft.is_dir(),
        is_symlink: ft.is_symlink(),
        dev: md.dev() as i32,
        ino: md.ino(),
        nlink: md.nlink() as u32,
        size_logical: md.size(),
        // Same st_blocks*512 derivation as read_dir_attrs (whose ALLOCSIZE path
        // is verified equal to this on APFS — see parse_record).
        size_blocks: md.blocks() * 512,
        mtime: md.mtime(),
        clone_id,
    })
}
