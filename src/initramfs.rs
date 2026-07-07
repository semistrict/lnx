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

    if path.exists()
        && stamp_path.exists()
        && fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str())
    {
        return Ok((path, false));
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
    fs::write(path, buf).with_context(|| format!("write {}", path.display()))?;
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
mod tests;
