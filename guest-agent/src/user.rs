use std::ffi::{CString, c_char, c_int, c_uint};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

pub const EXEC_USER: &str = "lnxuser";
pub const EXEC_HOME: &str = "/home/lnxuser";

unsafe extern "C" {
    fn chown(path: *const c_char, owner: c_uint, group: c_uint) -> c_int;
}

pub fn ensure_exec_user(uid: u32, gid: u32) {
    if uid == 0 {
        return;
    }
    if !file_contains_line_prefix("/etc/passwd", "lnxuser:") {
        if !create_exec_user_with_useradd(uid, gid) {
            append_file(
                "/etc/passwd",
                &format!("{EXEC_USER}:x:{uid}:{gid}::/home/{EXEC_USER}:/bin/bash\n"),
            );
            if !file_contains_line_prefix("/etc/shadow", "lnxuser:") {
                append_file("/etc/shadow", &format!("{EXEC_USER}:!::0:99999:7:::\n"));
            }
            if !file_contains_line_prefix("/etc/group", "lnxuser:") {
                append_file("/etc/group", &format!("{EXEC_USER}:x:{gid}:\n"));
            }
            let _ = fs::create_dir_all(EXEC_HOME);
        }
    }
    ensure_exec_user_skel(uid, gid);
    let _ = fs::create_dir_all("/etc/sudoers.d");
    let _ = fs::write(
        "/etc/sudoers.d/lnx",
        format!("{EXEC_USER} ALL=(ALL) NOPASSWD: ALL\n"),
    );
    let _ = fs::set_permissions("/etc/sudoers.d/lnx", fs::Permissions::from_mode(0o440));
}

fn create_exec_user_with_useradd(uid: u32, gid: u32) -> bool {
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
        .arg("/bin/bash")
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
