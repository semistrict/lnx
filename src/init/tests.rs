use super::*;
use std::{
    io::{Seek, Write},
    time::{SystemTime, UNIX_EPOCH},
};

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("lnx-{name}-{}-{unique}.ext4", std::process::id()));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn write_fake_ext4_with_state(path: &Path, log_block_size: u32, len: u64, state: u16) {
    let mut file = fs::File::create(path).expect("create fake ext4");
    file.set_len(len).expect("size fake ext4");
    let mut superblock = [0u8; EXT4_SUPERBLOCK_LEN];
    superblock[24..28].copy_from_slice(&log_block_size.to_le_bytes());
    superblock[56..58].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
    superblock[58..60].copy_from_slice(&state.to_le_bytes());
    file.seek(SeekFrom::Start(EXT4_SUPERBLOCK_OFFSET))
        .expect("seek superblock");
    file.write_all(&superblock).expect("write superblock");
}

fn write_fake_ext4(path: &Path, log_block_size: u32, len: u64) {
    write_fake_ext4_with_state(path, log_block_size, len, EXT4_VALID_FS);
}

#[test]
fn ext4_block_size_reads_superblock() {
    let image = TempFile::new("block-size");
    write_fake_ext4(image.path(), 4, 4096);

    assert_eq!(
        ext4_block_size(image.path()).expect("block size"),
        16 * 1024
    );
}

#[test]
fn validate_managed_rootfs_rejects_4k_ext4() {
    let image = TempFile::new("bad-block-size");
    write_fake_ext4(image.path(), 2, 4096);

    let error = validate_managed_rootfs(image.path(), 4096).expect_err("4K ext4 should fail");
    assert!(
        error.to_string().contains("expected 16384"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn validate_managed_rootfs_accepts_64g_16k_ext4() {
    let image = TempFile::new("good-rootfs");
    write_fake_ext4(image.path(), 4, DEFAULT_ROOTFS_SIZE);

    validate_managed_rootfs(image.path(), DEFAULT_ROOTFS_SIZE).expect("valid rootfs");
}

#[test]
fn ext4_error_state_is_rejected() {
    let image = TempFile::new("error-state");
    write_fake_ext4_with_state(image.path(), 4, 4096, EXT4_VALID_FS | EXT4_ERROR_FS);

    let error =
        ensure_ext4_has_no_errors(image.path(), "rootfs").expect_err("error state should fail");
    assert!(
        error.to_string().contains("marked with ext4 errors"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn release_cache_is_current_only_with_a_matching_stamp() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cached = temp.path().join("rootfs.ext4");
    fs::write(&cached, b"rootfs").expect("write cached rootfs");

    assert_eq!(recorded_release(&cached), None);

    record_release(&cached, "images-v0.6.0/rootfs.ext4.zst").expect("record release");
    assert_eq!(
        recorded_release(&cached).as_deref(),
        Some("images-v0.6.0/rootfs.ext4.zst")
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("rootfs.ext4.release")).expect("read stamp"),
        "images-v0.6.0/rootfs.ext4.zst\n"
    );
}

#[test]
fn matching_release_cache_is_reused_without_downloading() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cached = temp.path().join("rootfs.ext4");
    fs::write(&cached, b"rootfs").expect("write cached rootfs");
    record_release(&cached, "images-v9.9.9/rootfs.ext4.zst").expect("record release");

    ensure_release_asset(
        &cached,
        "rootfs.ext4.zst",
        "images-v9.9.9",
        CachePolicy::MatchRelease,
    )
    .expect("current cache needs no download");

    assert_eq!(fs::read(&cached).expect("read cache"), b"rootfs");
}

#[test]
fn kept_assets_are_reused_regardless_of_release() {
    let temp = tempfile::tempdir().expect("tempdir");
    let kernel = temp.path().join("vmlinuz");
    fs::write(&kernel, b"kernel").expect("write kernel");

    ensure_release_asset(
        &kernel,
        "vmlinuz.gz",
        "images-v9.9.9",
        CachePolicy::KeepExisting,
    )
    .expect("kept kernel needs no download");

    assert_eq!(fs::read(&kernel).expect("read kernel"), b"kernel");
}

#[test]
fn unique_siblings_differ_between_calls() {
    let path = Path::new("/base/cache/rootfs.ext4");
    let first = unique_sibling(path, "download");
    let second = unique_sibling(path, "download");
    assert_ne!(first, second);
    assert_eq!(first.parent(), path.parent());
}
