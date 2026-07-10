use std::process::Command;

fn sh(cmd: &str) {
    assert!(Command::new("sh").args(["-c", cmd]).status().unwrap().success());
}

#[test]
fn clone_ids_detect_clones_not_copies() {
    let dir = std::env::temp_dir().join(format!("duh-attrs-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok(); // stale leftovers from a killed prior run
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.bin");
    std::fs::write(&src, vec![0xABu8; 1 << 20]).unwrap();
    sh(&format!("cp -c {0}/src.bin {0}/clone.bin", dir.display()));
    sh(&format!("cp {0}/src.bin {0}/copy.bin", dir.display()));
    std::fs::create_dir(dir.join("subdir")).unwrap();
    std::os::unix::fs::symlink("src.bin", dir.join("link")).unwrap();

    let src_id = duh::attrs::get_clone_id(&src);
    let clone_id = duh::attrs::get_clone_id(&dir.join("clone.bin"));
    let copy_id = duh::attrs::get_clone_id(&dir.join("copy.bin"));

    assert!(src_id.is_some(), "no clone id on APFS?");
    assert_eq!(src_id, clone_id, "clone must share clone_id");
    assert_ne!(src_id, copy_id, "byte copy must NOT share clone_id");

    // bulk read agrees with per-path read, and sizes/inodes are sane
    let entries = duh::attrs::read_dir_attrs(&dir).unwrap();
    assert_eq!(entries.len(), 5);
    let e = entries.iter().find(|e| e.name == "src.bin").unwrap();
    assert_eq!(e.size_logical, 1 << 20);
    assert_eq!(e.size_blocks, 1 << 20, "allocated bytes must be st_blocks*512");
    assert_eq!(e.clone_id, src_id);
    assert!(!e.is_dir && !e.is_symlink && e.nlink == 1 && e.ino > 0);

    // directories and symlinks are flagged correctly; symlinks get no clone_id
    let d = entries.iter().find(|e| e.name == "subdir").unwrap();
    assert!(d.is_dir && !d.is_symlink && d.clone_id.is_none());
    let l = entries.iter().find(|e| e.name == "link").unwrap();
    assert!(l.is_symlink && !l.is_dir, "symlink must be flagged, not followed");
    assert!(l.clone_id.is_none(), "symlink must have no clone_id (Python parity)");

    std::fs::remove_dir_all(&dir).ok();
}
