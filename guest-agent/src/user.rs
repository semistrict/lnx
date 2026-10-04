use std::ffi::{CString, c_char, c_int, c_uint};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

pub const EXEC_USER: &str = "lnxuser";
pub const EXEC_HOME: &str = "/home/lnxuser";

/// The image's preferred login shell, the way adduser/useradd would pick it:
/// Debian's adduser.conf DSHELL, then adduser's documented default, then
/// useradd's SHELL default, then bash if the image ships it, then /bin/sh.
pub fn default_image_shell() -> String {
    if let Some(shell) = adduser_shell_from_config("/etc/adduser.conf") {
        return shell;
    }
    if let Some(shell) = shell_from_config("/etc/default/useradd", "SHELL=") {
        return shell;
    }
    if fs::metadata("/bin/bash").is_ok() {
        return "/bin/bash".to_string();
    }
    "/bin/sh".to_string()
}

fn adduser_shell_from_config(path: &str) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    adduser_shell_from_config_contents(&contents)
}

fn adduser_shell_from_config_contents(contents: &str) -> Option<String> {
    shell_from_config_contents(contents, "DSHELL=").or_else(|| {
        contents.lines().find_map(|line| {
            let line = line.trim().strip_prefix('#')?.trim();
            let default = line.strip_prefix("Default:")?.trim();
            shell_from_config_line(default, "DSHELL=")
        })
    })
}

fn shell_from_config(path: &str, key: &str) -> Option<String> {
    shell_from_config_contents(&fs::read_to_string(path).ok()?, key)
}

fn shell_from_config_contents(contents: &str, key: &str) -> Option<String> {
    contents
        .lines()
        .find_map(|line| shell_from_config_line(line.trim(), key))
}

fn shell_from_config_line(line: &str, key: &str) -> Option<String> {
    let shell = line.strip_prefix(key)?.trim_matches('"');
    (shell.starts_with('/') && fs::metadata(shell).is_ok()).then(|| shell.to_string())
}

/// Login shell for a uid per /etc/passwd, falling back to the image default.
pub fn login_shell_for_uid(uid: u32) -> String {
    let uid = uid.to_string();
    fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|passwd| {
            passwd.lines().find_map(|line| {
                let fields = line.split(':').collect::<Vec<_>>();
                (fields.len() >= 7 && fields[2] == uid && !fields[6].is_empty())
                    .then(|| fields[6].to_string())
            })
        })
        .unwrap_or_else(default_image_shell)
}

unsafe extern "C" {
    fn chown(path: *const c_char, owner: c_uint, group: c_uint) -> c_int;
}

/// The exec identity last set up, so later commands as the same user skip
/// the work. Holding it also serializes setup: commands start on their own
/// threads, and two of them editing /etc/group at once could each write back
/// a copy missing the other's view of the file (dpkg then fails on groups
/// that "got removed").
static EXEC_IDENTITY: Mutex<Option<(u32, u32, String)>> = Mutex::new(None);

pub fn ensure_exec_user(uid: u32, gid: u32, group: &str) {
    if uid == 0 {
        return;
    }
    let mut done = EXEC_IDENTITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let identity = (uid, gid, group.to_string());
    if done.as_ref() == Some(&identity) {
        return;
    }
    set_up_exec_user(uid, gid, group);
    *done = Some(identity);
}

fn set_up_exec_user(uid: u32, gid: u32, group: &str) {
    ensure_exec_group(gid, group);
    let shell = default_image_shell();
    if !file_contains_line_prefix("/etc/passwd", "lnxuser:") {
        if !create_exec_user_with_useradd(uid, gid, &shell) {
            append_file(
                "/etc/passwd",
                &format!("{EXEC_USER}:x:{uid}:{gid}::/home/{EXEC_USER}:{shell}\n"),
            );
            if !file_contains_line_prefix("/etc/shadow", "lnxuser:") {
                append_file("/etc/shadow", &format!("{EXEC_USER}:!::0:99999:7:::\n"));
            }
            if !file_contains_line_prefix("/etc/group", "lnxuser:") {
                append_file("/etc/group", &format!("{EXEC_USER}:x:{gid}:\n"));
            }
            let _ = fs::create_dir_all(EXEC_HOME);
        }
    } else {
        ensure_exec_user_shell(&shell);
    }
    ensure_exec_user_skel(uid, gid);
    let _ = install_sudoers_dropin();
}

fn install_sudoers_dropin() -> std::io::Result<()> {
    install_sudoers_dropin_at(
        "/etc/sudoers.d",
        "/etc/sudoers.d/lnx",
        "/etc/sudoers.d/.lnx.tmp",
    )
}

fn install_sudoers_dropin_at(dir: &str, path: &str, tmp: &str) -> std::io::Result<()> {
    let contents = format!("{EXEC_USER} ALL=(ALL) NOPASSWD: ALL\n");
    if sudoers_dropin_is_current(path, &contents) {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let _ = fs::remove_file(tmp);
    {
        let mut file = OpenOptions::new().write(true).create_new(true).open(tmp)?;
        file.write_all(contents.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(0o440))?;
        file.sync_all()?;
    }
    fs::rename(tmp, path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn sudoers_dropin_is_current(path: &str, contents: &str) -> bool {
    let Ok(existing) = fs::read_to_string(path) else {
        return false;
    };
    if existing != contents {
        return false;
    }
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o777 == 0o440)
        .unwrap_or(false)
}

fn ensure_exec_user_shell(shell: &str) {
    if passwd_shell_for_user(EXEC_USER).as_deref() == Some(shell) {
        return;
    }
    let changed = Command::new("/usr/sbin/usermod")
        .arg("-s")
        .arg(shell)
        .arg(EXEC_USER)
        .status()
        .is_ok_and(|status| status.success());
    if !changed {
        rewrite_passwd_shell(EXEC_USER, shell);
    }
}

fn passwd_shell_for_user(user: &str) -> Option<String> {
    fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find_map(|line| {
            let fields = line.split(':').collect::<Vec<_>>();
            (fields.len() >= 7 && fields[0] == user).then(|| fields[6].to_string())
        })
}

fn rewrite_passwd_shell(user: &str, shell: &str) {
    let Ok(contents) = fs::read_to_string("/etc/passwd") else {
        return;
    };
    let rewritten = contents
        .lines()
        .map(|line| {
            let mut fields = line.split(':').collect::<Vec<_>>();
            if fields.len() >= 7 && fields[0] == user {
                fields[6] = shell;
                fields.join(":")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let _ = replace_file("/etc/passwd", &(rewritten + "\n"));
}

/// Give the exec user's group a name in the guest: the host's name for it
/// when the guest has no group with that id. A guest group that already has
/// the id keeps its name: renaming a distribution's system group (macOS's
/// gid 20 `staff` is Ubuntu's `dialout`) breaks package scripts that expect
/// it, which then leave dpkg unable to install anything.
fn ensure_exec_group(gid: u32, host_name: &str) {
    if !is_portable_group_name(host_name) || group_name_for_gid(gid).is_some() {
        return;
    }
    let added = Command::new("/usr/sbin/groupadd")
        .arg("-g")
        .arg(gid.to_string())
        .arg(host_name)
        .status()
        .is_ok_and(|status| status.success());
    if !added {
        append_file("/etc/group", &format!("{host_name}:x:{gid}:\n"));
    }
}

fn is_portable_group_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 32
        && (first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn group_name_for_gid(gid: u32) -> Option<String> {
    let gid = gid.to_string();
    fs::read_to_string("/etc/group")
        .ok()?
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let _password = fields.next()?;
            (fields.next()? == gid).then(|| name.to_string())
        })
}

/// Replaces `path` with `contents` in one step (a sibling temporary file
/// renamed over it, keeping the mode), so no reader ever sees it half
/// written.
fn replace_file(path: &str, contents: &str) -> std::io::Result<()> {
    let path = Path::new(path);
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.lnx-tmp"));
    let mode = fs::metadata(path).map(|metadata| metadata.permissions().mode())?;
    let _ = fs::remove_file(&tmp);
    {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn create_exec_user_with_useradd(uid: u32, gid: u32, shell: &str) -> bool {
    let _ = Command::new("/usr/sbin/groupadd")
        .arg("-g")
        .arg(gid.to_string())
        .arg(EXEC_USER)
        .status();
    Command::new("/usr/sbin/useradd")
        .arg("-m")
        .arg("-d")
        .arg(EXEC_HOME)
        .arg("-s")
        .arg(shell)
        .arg("-u")
        .arg(uid.to_string())
        .arg("-g")
        .arg(gid.to_string())
        .arg(EXEC_USER)
        .status()
        .is_ok_and(|status| status.success())
}

fn ensure_exec_user_skel(uid: u32, gid: u32) {
    let _ = fs::create_dir_all(EXEC_HOME);
    for name in [".bashrc", ".profile", ".bash_logout"] {
        let dest = format!("{EXEC_HOME}/{name}");
        if fs::metadata(&dest).is_err() {
            let _ = fs::copy(format!("/etc/skel/{name}"), &dest);
        }
        chown_path(&dest, uid, gid);
    }
    chown_path(EXEC_HOME, uid, gid);
}

fn append_file(path: &str, line: &str) {
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn file_contains_line_prefix(path: &str, prefix: &str) -> bool {
    fs::read_to_string(path)
        .map(|contents| contents.lines().any(|line| line.starts_with(prefix)))
        .unwrap_or(false)
}

fn chown_path(path: &str, uid: u32, gid: u32) {
    if let Ok(path) = CString::new(path) {
        unsafe {
            chown(path.as_ptr(), uid, gid);
        }
    }
}

#[cfg(test)]
mod tests;
