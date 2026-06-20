use std::ffi::{CString, c_char, c_int, c_uint};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

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

pub fn ensure_exec_user(uid: u32, gid: u32, group: &str) {
    if uid == 0 {
        return;
    }
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
    let _ = fs::write("/etc/passwd", rewritten + "\n");
}

/// Make the guest's group for `gid` carry the host's name for it, so shared
/// files list the same owner group on both sides (e.g. gid 20 is `staff` on
/// macOS but ships as `dialout` in the rootfs). Renames an existing group or
/// creates a missing one; host names that are not portable Linux group names
/// leave the guest untouched.
fn ensure_exec_group(gid: u32, host_name: &str) {
    if !is_portable_group_name(host_name) {
        return;
    }
    match group_name_for_gid(gid) {
        Some(name) if name == host_name => {}
        Some(name) => {
            let renamed = Command::new("/usr/sbin/groupmod")
                .arg("-n")
                .arg(host_name)
                .arg(&name)
                .status()
                .is_ok_and(|status| status.success());
            if !renamed {
                rename_group_entry(&name, host_name);
            }
        }
        None => {
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

fn rename_group_entry(old: &str, new: &str) {
    let Ok(contents) = fs::read_to_string("/etc/group") else {
        return;
    };
    let prefix = format!("{old}:");
    let renamed = contents
        .lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix(&prefix) {
                format!("{new}:{rest}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let _ = fs::write("/etc/group", renamed + "\n");
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
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn adduser_shell_uses_active_dshell() {
        assert_eq!(
            adduser_shell_from_config_contents(
                r#"
# Default: DSHELL=/bin/bash
DSHELL=/bin/sh
"#,
            ),
            Some("/bin/sh".to_string())
        );
    }

    #[test]
    fn adduser_shell_uses_documented_default() {
        assert_eq!(
            adduser_shell_from_config_contents(
                r#"
# The DSHELL variable specifies the default login shell.
# Default: DSHELL=/bin/bash
#DSHELL=/bin/bash
"#,
            ),
            Some("/bin/bash".to_string())
        );
    }

    #[test]
    fn sudoers_dropin_is_installed_atomically_with_mode() {
        let root = std::env::temp_dir().join(format!(
            "lnx-sudoers-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let dir = root.join("sudoers.d");
        let path = dir.join("lnx");
        let tmp = dir.join(".lnx.tmp");

        install_sudoers_dropin_at(
            dir.to_str().expect("dir path"),
            path.to_str().expect("sudoers path"),
            tmp.to_str().expect("tmp path"),
        )
        .expect("install sudoers");

        assert_eq!(
            fs::read_to_string(&path).expect("read sudoers"),
            "lnxuser ALL=(ALL) NOPASSWD: ALL\n"
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("stat sudoers")
                .permissions()
                .mode()
                & 0o777,
            0o440
        );
        assert!(!tmp.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn current_sudoers_dropin_skips_rewrite() {
        let root = std::env::temp_dir().join(format!(
            "lnx-sudoers-current-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let dir = root.join("sudoers.d");
        let path = dir.join("lnx");
        let tmp = dir.join(".lnx.tmp");
        fs::create_dir_all(&dir).expect("create dir");
        fs::write(&path, "lnxuser ALL=(ALL) NOPASSWD: ALL\n").expect("write sudoers");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o440)).expect("chmod sudoers");
        fs::write(&tmp, "stale temp").expect("write stale temp");

        install_sudoers_dropin_at(
            dir.to_str().expect("dir path"),
            path.to_str().expect("sudoers path"),
            tmp.to_str().expect("tmp path"),
        )
        .expect("install sudoers");

        assert_eq!(
            fs::read_to_string(&tmp).expect("read stale temp"),
            "stale temp"
        );

        let _ = fs::remove_dir_all(root);
    }
}
