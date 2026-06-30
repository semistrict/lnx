use std::{path::Path, sync::OnceLock};

use anyhow::Result;

const COMPAT_NET_FEATURES: u32 = (1 << 0) | (1 << 1) | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 14);
const NET_FLAG_VFKIT: u32 = 1 << 0;
const NET_FLAG_DHCP_CLIENT: u32 = 1 << 1;
const VIRTIOFS_DAX_WINDOW_BYTES: u64 = 8 << 30;
/// Opt back into the old host-share DAX path. Writable host-share DAX can
/// wedge SQLite WAL close/unmap paths on macOS, so the default is the safer
/// non-DAX virtio-fs mount.
pub(crate) const HOST_SHARE_DAX_ENV: &str = "LNX_HOST_SHARE_DAX";

pub(crate) fn set_log_level_once(level: u32) -> Result<()> {
    static LOG_LEVEL: OnceLock<u32> = OnceLock::new();
    if LOG_LEVEL.set(level).is_err() {
        return Ok(());
    }
    libkrun::Context::set_log_level(level)?;
    Ok(())
}

pub(crate) fn add_root_pmem(ctx: &libkrun::Context, rootfs: &Path) -> Result<()> {
    ctx.add_pmem("rootfs", rootfs, false)?;
    Ok(())
}

pub(crate) fn add_virtiofs(
    ctx: &libkrun::Context,
    tag: &str,
    path: &Path,
    read_only: bool,
) -> Result<()> {
    ctx.add_virtiofs(
        tag,
        Some(path),
        host_share_dax_window_bytes(),
        read_only,
        false,
    )?;
    Ok(())
}

pub(crate) fn add_host_virtiofs(
    ctx: &libkrun::Context,
    tag: &str,
    path: &Path,
    write_allowlist: &[String],
    unshare_dir: &Path,
) -> Result<()> {
    ctx.add_virtiofs(tag, Some(path), host_share_dax_window_bytes(), false, true)?;
    ctx.set_virtiofs_unshare_dir(tag, unshare_dir)?;
    set_host_virtiofs_write_allowlist(ctx, tag, write_allowlist)
}

#[allow(dead_code)]
pub(crate) fn set_host_virtiofs_write_allowlist(
    ctx: &libkrun::Context,
    tag: &str,
    paths: &[String],
) -> Result<()> {
    ctx.set_virtiofs_write_allowlist(tag, paths.iter().map(Path::new).map(Into::into).collect())?;
    Ok(())
}

pub(crate) fn add_vsock_connector(ctx: &libkrun::Context, port: u32, socket: &Path) -> Result<()> {
    ctx.add_vsock_port(port, socket, false)?;
    Ok(())
}

pub(crate) fn add_vhost_user_virtiofs(
    ctx: &libkrun::Context,
    tag: &str,
    socket: &Path,
) -> Result<()> {
    ctx.add_vhost_user_virtiofs(tag, socket)?;
    Ok(())
}

pub(crate) fn add_gvproxy_network(ctx: &libkrun::Context, socket: &Path) -> Result<()> {
    let mac = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
    ctx.add_net_unixgram(
        socket,
        mac,
        COMPAT_NET_FEATURES,
        NET_FLAG_VFKIT | NET_FLAG_DHCP_CLIENT,
    )?;
    Ok(())
}

pub(crate) fn host_share_dax_enabled() -> bool {
    matches!(
        std::env::var(HOST_SHARE_DAX_ENV).as_deref(),
        Ok("1" | "true" | "on" | "yes")
    )
}

fn host_share_dax_window_bytes() -> u64 {
    if host_share_dax_enabled() {
        VIRTIOFS_DAX_WINDOW_BYTES
    } else {
        0
    }
}
