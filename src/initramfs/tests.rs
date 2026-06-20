use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lnx-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn write_from_agent_creates_expected_initramfs_entries() {
    let temp = TempDir::new("initramfs-entries");
    let (initrd, rebuilt) = write_from_agent(b"agent-bytes", "source-a", temp.path().join("run"))
        .expect("write initramfs");

    assert!(rebuilt);
    let bytes = fs::read(&initrd).expect("read initramfs");
    assert_eq!(bytes.len() % 512, 0);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("init\0"));
    assert!(text.contains("lnx-agent\0"));
    assert!(text.contains("lnxctl\0"));
    assert!(text.contains("TRAILER!!!\0"));
    assert!(text.contains("agent-bytes"));
}

#[test]
fn write_from_agent_reuses_cache_until_source_stamp_changes() {
    let temp = TempDir::new("initramfs-cache");
    let run_dir = temp.path().join("run");

    let (initrd, rebuilt) =
        write_from_agent(b"first-agent", "source-a", run_dir.clone()).expect("first write");
    assert!(rebuilt);
    let first = fs::read(&initrd).expect("read first");

    let (_, rebuilt) =
        write_from_agent(b"different-binary", "source-a", run_dir.clone()).expect("cached write");
    assert!(!rebuilt);
    assert_eq!(fs::read(&initrd).expect("read cached"), first);

    let (_, rebuilt) =
        write_from_agent(b"second-agent", "source-b", run_dir).expect("rewritten initramfs");
    assert!(rebuilt);
    let second = fs::read(&initrd).expect("read second");
    assert_ne!(second, first);
    assert!(String::from_utf8_lossy(&second).contains("second-agent"));
}
