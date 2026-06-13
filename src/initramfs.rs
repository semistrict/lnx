use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

pub fn write_from_agent(
    agent: &[u8],
    agent_source_stamp: &str,
    dir: PathBuf,
) -> Result<(PathBuf, bool)> {
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("initramfs.cpio");
    let stamp_path = dir.join("initramfs.stamp");
    let stamp = stamp(agent_source_stamp);

    if path.exists() && stamp_path.exists() {
        if fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str()) {
            return Ok((path, false));
        }
    }

    write_agent(agent, &path)?;
    fs::write(&stamp_path, stamp).with_context(|| format!("write {}", stamp_path.display()))?;
    Ok((path, true))
}

fn write_agent(agent: &[u8], path: &Path) -> Result<()> {
    let mut buf = Vec::new();
    entry(&mut buf, "init", agent, 0o100755)?;
    entry(&mut buf, "lnx-agent", agent, 0o100755)?;
    entry(&mut buf, "lnxctl", agent, 0o100755)?;
    entry(&mut buf, "TRAILER!!!", &[], 0)?;
    if buf.len() % 512 != 0 {
        buf.resize(buf.len() + (512 - buf.len() % 512), 0);
    }
    fs::write(&path, buf).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn stamp(agent_source_stamp: &str) -> String {
    format!("source={agent_source_stamp}\n")
}

fn entry(buf: &mut Vec<u8>, name: &str, data: &[u8], mode: u32) -> Result<()> {
    let namesize = name.len() + 1;
    write!(
        buf,
        "070701\
         {ino:08X}{mode:08X}{uid:08X}{gid:08X}{nlink:08X}{mtime:08X}{filesize:08X}\
         {devmajor:08X}{devminor:08X}{rdevmajor:08X}{rdevminor:08X}{namesize:08X}{check:08X}",
        ino = 1,
        mode = mode,
        uid = 0,
        gid = 0,
        nlink = 1,
        mtime = 0,
        filesize = data.len(),
        devmajor = 0,
        devminor = 0,
        rdevmajor = 0,
        rdevminor = 0,
        namesize = namesize,
        check = 0,
    )?;
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    pad4(buf);
    buf.extend_from_slice(data);
    pad4(buf);
    Ok(())
}

fn pad4(buf: &mut Vec<u8>) {
    if buf.len() % 4 != 0 {
        buf.resize(buf.len() + (4 - buf.len() % 4), 0);
    }
}

#[cfg(test)]
mod tests {
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
            let path =
                std::env::temp_dir().join(format!("lnx-{name}-{}-{unique}", std::process::id()));
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
        let (initrd, rebuilt) =
            write_from_agent(b"agent-bytes", "source-a", temp.path().join("run"))
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

        let (_, rebuilt) = write_from_agent(b"different-binary", "source-a", run_dir.clone())
            .expect("cached write");
        assert!(!rebuilt);
        assert_eq!(fs::read(&initrd).expect("read cached"), first);

        let (_, rebuilt) =
            write_from_agent(b"second-agent", "source-b", run_dir).expect("rewritten initramfs");
        assert!(rebuilt);
        let second = fs::read(&initrd).expect("read second");
        assert_ne!(second, first);
        assert!(String::from_utf8_lossy(&second).contains("second-agent"));
    }
}
