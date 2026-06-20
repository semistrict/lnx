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
