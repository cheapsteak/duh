//! Per-entry filesystem metadata for the scanner, with a platform backend:
//!
//! - **macOS** (`attrs/macos.rs`): `getattrlistbulk` / `getattrlist` FFI with APFS
//!   clone-ID detection.
//! - **Linux** (`attrs/linux.rs`): plain `readdir` + `fstatat` (via
//!   `std::fs::DirEntry::metadata`). Hardlink families are detected by
//!   `(dev, ino)` + `nlink` exactly as on macOS; there is **no** clone/reflink
//!   detection (btrfs/XFS reflinks expose no clone id), so `clone_id` is always
//!   `None` and reflinked files count as fully owned.
//!
//! Both backends expose the same API: [`EntryAttrs`], `read_dir_attrs`,
//! `get_clone_id`, and [`stat_root`].

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{get_clone_id, read_dir_attrs};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{get_clone_id, read_dir_attrs};

/// Whether this build can detect clone families (APFS clone ids). `false` on
/// Linux, where reflinked files are indistinguishable from independent copies.
pub const CLONE_DETECTION: bool = cfg!(target_os = "macos");

/// Attributes for a single directory entry (or scan root).
///
/// `size_blocks` is allocated bytes with `st_blocks * 512` semantics everywhere
/// (Python parity). For regular files/symlinks it comes from `ATTR_FILE_ALLOCSIZE`,
/// which was empirically verified equal to `st_blocks * 512` on APFS, including
/// for decmpfs-compressed files (see comment in `parse_record`); directories and
/// `stat_root` derive it from lstat's `st_blocks * 512` directly. On Linux it is
/// `st_blocks * 512` for every entry.
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
        // Linux `st_dev` is 64-bit; the `as i32` truncation is shared with
        // `linux::read_dir_attrs`, so every comparison sees the same value.
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
