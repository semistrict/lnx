use super::*;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("lnx-{name}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn allocated_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).expect("stat").blocks() * 512
}

#[test]
fn copies_sparse_file_without_materializing_holes() {
    let dir = TempDir::new("sparse-copy");
    let src = dir.path.join("src.img");
    let dst = dir.path.join("dst.img");

    let mut file = fs::File::create(&src).expect("create src");
    file.set_len(64 * 1024 * 1024).expect("set_len");
    file.seek(SeekFrom::Start(9 * 1024 * 1024)).expect("seek");
    file.write_all(b"data-in-the-middle").expect("write");
    file.sync_all().expect("sync");
    drop(file);

    clone_or_copy_file(&src, &dst).expect("clone");

    let src_bytes = fs::read(&src).expect("read src");
    let dst_bytes = fs::read(&dst).expect("read dst");
    assert_eq!(src_bytes, dst_bytes, "contents must match");
    assert!(
        allocated_bytes(&dst) < 16 * 1024 * 1024,
        "destination must stay sparse, allocated {} bytes",
        allocated_bytes(&dst)
    );
}

#[test]
fn preserves_read_only_permissions_and_timestamps() {
    let dir = TempDir::new("sparse-copy-metadata");
    let src = dir.path.join("src.img");
    let dst = dir.path.join("dst.img");
    fs::write(&src, b"snapshot data").expect("write source");
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::File::open(&src)
        .expect("open source")
        .set_times(fs::FileTimes::new().set_modified(modified))
        .expect("set source timestamp");
    fs::set_permissions(&src, fs::Permissions::from_mode(0o444)).expect("make source read-only");

    clone_or_copy_file(&src, &dst).expect("clone read-only source");

    let source = fs::metadata(&src).expect("stat source");
    let destination = fs::metadata(&dst).expect("stat destination");
    assert_eq!(destination.permissions().mode() & 0o777, 0o444);
    assert_eq!(destination.modified().expect("destination mtime"), modified);
    assert_eq!(destination.modified().unwrap(), source.modified().unwrap());
}
