//! Linux backend: `readdir` + per-entry `fstatat(AT_SYMLINK_NOFOLLOW)`.
//!
//! `std::fs::DirEntry::metadata` on Linux stats relative to the open directory
//! fd without following symlinks, so this is one `getdents64` stream plus one
//! `statx`/`fstatat` per entry — no path re-resolution from the root.
//!
//! There is no clone-id equivalent on Linux: btrfs/XFS reflinks (`FICLONE`,
//! `cp --reflink`) share extents but expose no stable family id (FIEMAP can only
//! say an extent is "shared", not with whom). `clone_id` is therefore always
//! `None`, and freeable treats reflinked files as fully owned — it overstates
//! freeable for them. Hardlinks are fully supported via `(dev, ino)` + `nlink`.

use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::EntryAttrs;

/// Read one directory's entries (excluding `.`/`..`) with lstat semantics.
///
/// An entry that disappears between `readdir` and `stat` (a racing delete) is
/// skipped silently; any other per-entry stat failure is reported on stderr and
/// the entry skipped — a skipped entry beats a failed directory. Failure to open
/// or read the directory itself is returned to the caller, matching the macOS
/// backend.
pub fn read_dir_attrs(dir: &Path) -> std::io::Result<Vec<EntryAttrs>> {
    let mut out = Vec::new();
    for ent in std::fs::read_dir(dir)? {
        let ent = ent?;
        let md = match ent.metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                eprintln!(
                    "duh: skipping {}: {e}",
                    dir.join(ent.file_name()).display()
                );
                continue;
            }
        };
        let ft = md.file_type();
        out.push(EntryAttrs {
            name: ent.file_name(),
            is_dir: ft.is_dir(),
            is_symlink: ft.is_symlink(),
            // 64-bit st_dev truncated the same way as `stat_root` (see there).
            dev: md.dev() as i32,
            ino: md.ino(),
            nlink: md.nlink() as u32,
            size_logical: md.size(),
            size_blocks: md.blocks() * 512,
            mtime: md.mtime(),
            clone_id: None,
        });
    }
    Ok(out)
}

/// No clone ids on Linux (see module docs). Always `None`.
pub fn get_clone_id(_path: &Path) -> Option<u64> {
    None
}
