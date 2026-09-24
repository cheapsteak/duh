//! Platform-neutral checks of the directory reader: hardlink identity, sizes,
//! and entry typing. Runs on both macOS and Linux.

#[test]
fn read_dir_attrs_reports_hardlinks_sizes_and_types() {
    let dir = std::env::temp_dir().join(format!("duh-hl-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok(); // stale leftovers from a killed prior run
    std::fs::create_dir_all(&dir).unwrap();

    let data = vec![0x5Au8; 1 << 20];
    std::fs::write(dir.join("src.bin"), &data).unwrap();
    std::fs::hard_link(dir.join("src.bin"), dir.join("link.bin")).unwrap();
    std::fs::write(dir.join("copy.bin"), &data).unwrap();
    std::fs::create_dir(dir.join("subdir")).unwrap();
    std::os::unix::fs::symlink("src.bin", dir.join("sym")).unwrap();

    let entries = duh::attrs::read_dir_attrs(&dir).unwrap();
    assert_eq!(entries.len(), 5, "no . or .. and nothing missing: {entries:?}");
    let get = |n: &str| entries.iter().find(|e| e.name == n).unwrap();

    let (src, link, copy) = (get("src.bin"), get("link.bin"), get("copy.bin"));
    assert_eq!((src.dev, src.ino), (link.dev, link.ino), "hardlinks share (dev, ino)");
    assert_eq!((src.nlink, link.nlink), (2, 2));
    assert_ne!(copy.ino, src.ino, "a byte copy is its own inode");
    assert_eq!(copy.nlink, 1);
    for e in [src, link, copy] {
        assert!(!e.is_dir && !e.is_symlink);
        assert_eq!(e.size_logical, 1 << 20);
        assert!(e.size_blocks >= 1 << 20, "allocated bytes are st_blocks*512");
    }

    let d = get("subdir");
    assert!(d.is_dir && !d.is_symlink && d.clone_id.is_none());
    let l = get("sym");
    assert!(l.is_symlink && !l.is_dir, "symlink must be flagged, not followed");
    assert!(l.clone_id.is_none());

    // Root stat agrees with the bulk reader on the device.
    let root = duh::attrs::stat_root(&dir).unwrap();
    assert!(root.is_dir);
    assert_eq!(root.dev, src.dev);

    #[cfg(target_os = "linux")]
    assert!(
        entries.iter().all(|e| e.clone_id.is_none()),
        "Linux has no clone ids"
    );

    std::fs::remove_dir_all(&dir).ok();
}
