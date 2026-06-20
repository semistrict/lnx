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

fn write_fake_ext4(path: &Path, log_block_size: u32, len: u64) {
    let mut file = fs::File::create(path).expect("create fake ext4");
    file.set_len(len).expect("size fake ext4");
    let mut superblock = [0u8; EXT4_SUPERBLOCK_LEN];
    superblock[24..28].copy_from_slice(&log_block_size.to_le_bytes());
    superblock[56..58].copy_from_slice(&0xEF53u16.to_le_bytes());
    file.seek(SeekFrom::Start(EXT4_SUPERBLOCK_OFFSET))
        .expect("seek superblock");
    file.write_all(&superblock).expect("write superblock");
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
