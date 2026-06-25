// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::btree_map;
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::null_mut;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crossbeam_channel::{Sender, unbounded};
use nix::errno::Errno;
use serde::{Deserialize, Serialize};
use utils::worker_message::WorkerMessage;

use crate::virtio::fs::filesystem::SecContext;

use super::super::super::linux_errno::{LINUX_ERANGE, linux_error};
use super::super::bindings;
use super::super::filesystem::{
    Context, DirEntry, Entry, ExportTable, Extensions, FileSystem, FsOptions, GetxattrReply,
    ListxattrReply, OpenOptions, SetattrValid, ZeroCopyReader, ZeroCopyWriter,
};
use super::super::fuse;
use super::super::inode_alloc::InodeAllocator;
use super::super::multikey::MultikeyBTreeMap;

const XATTR_KEY: &[u8] = b"user.containers.override_stat\0";
const SECURITY_CAPABILITY: &[u8] = b"security.capability\0";

const DAX_MAPPING_ALIGNMENT: usize = 2 * 1024 * 1024;
const UID_MAX: u32 = u32::MAX - 1;

type Inode = u64;
type Handle = u64;

#[derive(Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
struct InodeAltKey {
    ino: u64,
    dev: i32,
}

struct InodeData {
    inode: Inode,
    ino: u64,
    dev: i32,
    refcount: AtomicU64,
    unlinked_fd: AtomicI64,
}

enum InodeHandle {
    Fd(RawFd),
    Path(CString),
}

enum OverlayPath {
    Lower(CString),
    Upper(CString),
    Whiteout,
}

struct CachedDirEntry {
    ino: bindings::ino64_t,
    name: Box<[u8]>,
    type_: u8,
}

struct DirStream {
    entries: Vec<CachedDirEntry>,
    ready: bool,
}

impl DirStream {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            ready: false,
        }
    }

    fn get_entry<'a>(&'a self, offset: u64) -> Option<DirEntry<'a>> {
        self.entries.get(offset as usize).map(|e| DirEntry {
            ino: e.ino,
            // offset points to the next entry, not the current one
            offset: offset + 1,
            type_: u32::from(e.type_),
            name: &e.name,
        })
    }

    fn fill_from_fd(&mut self, fd: RawFd) -> io::Result<()> {
        // fdopendir() takes ownership of the fd, so we need to obtain a new one
        // to be donated.
        let newfd = unsafe { libc::dup(fd) };
        if newfd < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }
        let dir = unsafe { libc::fdopendir(newfd) };
        if dir.is_null() {
            let err = io::Error::last_os_error();
            let _ = unsafe { libc::close(newfd) };
            return Err(linux_error(err));
        }

        loop {
            // To detect if error happened in readdir we should clear errno
            // before the call and then verify it after
            Errno::clear();
            let dentry = unsafe { libc::readdir(dir) };
            if dentry.is_null() {
                let errno = Errno::last_raw();
                if errno != 0 {
                    let err = io::Error::from_raw_os_error(errno);
                    let _ = unsafe { libc::closedir(dir) };
                    // Error happened in readdir, but we keep the entries we
                    // already read to handle the partial read.
                    return Err(linux_error(err));
                }
                break;
            }
            // SAFETY: dentry is not null.
            // We trust macOS to return correct number of bytes for the name
            // length. The lifetime of a slice does not escape the unsafe block
            // as we copy the data into box right away.
            let name = unsafe {
                let name_len = usize::from((*dentry).d_namlen);
                let name_ptr = (*dentry).d_name.as_ptr().cast();
                let name = std::slice::from_raw_parts(name_ptr, name_len);

                if name == b"." || name == b".." {
                    continue;
                }
                Box::<[u8]>::from(name)
            };

            // SAFETY: dentry is not null.
            let ino = unsafe { (*dentry).d_ino };
            // SAFETY: dentry is not null. The entry types use the same
            // exact constants (`libc::DT_*`) on macOS, Linux, and FUSE.
            let type_ = unsafe { (*dentry).d_type };

            self.entries.push(CachedDirEntry { ino, name, type_ });
        }

        unsafe { libc::closedir(dir) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command as ProcessCommand;
    use std::sync::{Arc, RwLock};

    use super::{
        CachePolicy, Config, Context, DaxMappingSnapshot, Extensions, FileSystem, FsOptions,
        PassthroughFs, SetattrValid, fuse,
    };
    use crate::virtio::fs::bindings;
    use crate::virtio::fs::inode_alloc::InodeAllocator;

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("libkrun-virtiofs-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_ctx() -> Context {
        Context {
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            pid: std::process::id() as libc::pid_t,
        }
    }

    fn cstr(value: &str) -> CString {
        CString::new(value).unwrap()
    }

    fn new_fs(root: &Path, allowlist: Vec<PathBuf>) -> PassthroughFs {
        let fs = PassthroughFs::new(
            Config {
                root_dir: root.to_string_lossy().into_owned(),
                write_allowlist: Some(Arc::new(RwLock::new(allowlist))),
                ..Default::default()
            },
            Arc::new(InodeAllocator::new()),
        )
        .unwrap();
        fs.init(FsOptions::empty()).unwrap();
        fs
    }

    fn new_fs_with_unshare(root: &Path, unshare_dir: &Path) -> PassthroughFs {
        let fs = PassthroughFs::new(
            Config {
                root_dir: root.to_string_lossy().into_owned(),
                write_allowlist: Some(Arc::new(RwLock::new(vec![PathBuf::from(".")]))),
                unshare_dir: Some(unshare_dir.to_path_buf()),
                ..Default::default()
            },
            Arc::new(InodeAllocator::new()),
        )
        .unwrap();
        fs.init(FsOptions::empty()).unwrap();
        fs
    }

    fn init_git_repo(path: &Path, gitignore: &str) {
        let status = ProcessCommand::new("git")
            .arg("init")
            .arg("-q")
            .arg(path)
            .status()
            .expect("run git init");
        assert!(status.success());
        fs::write(path.join(".gitignore"), gitignore).unwrap();
    }

    #[test]
    fn cache_policy_never_opens_files_with_direct_io() {
        let temp = TempRoot::new("cache-policy-never");
        fs::write(temp.path().join("file.txt"), b"contents").unwrap();
        let fs = PassthroughFs::new(
            Config {
                root_dir: temp.path().to_string_lossy().into_owned(),
                cache_policy: CachePolicy::Never,
                ..Default::default()
            },
            Arc::new(InodeAllocator::new()),
        )
        .unwrap();
        fs.init(FsOptions::empty()).unwrap();
        let ctx = test_ctx();

        let file = fs.lookup(ctx, fuse::ROOT_ID, &cstr("file.txt")).unwrap();
        let (_, file_opts) = fs
            .open(ctx, file.inode, false, libc::O_RDONLY as u32)
            .unwrap();
        assert!(file_opts.contains(fuse::OpenOptions::DIRECT_IO));
        assert!(!file_opts.contains(fuse::OpenOptions::KEEP_CACHE));
    }

    #[test]
    fn cache_policy_auto_uses_close_to_open_consistency() {
        let temp = TempRoot::new("cache-policy-auto");
        fs::write(temp.path().join("file.txt"), b"contents").unwrap();
        let fs = PassthroughFs::new(
            Config {
                root_dir: temp.path().to_string_lossy().into_owned(),
                cache_policy: CachePolicy::Auto,
                ..Default::default()
            },
            Arc::new(InodeAllocator::new()),
        )
        .unwrap();
        fs.init(FsOptions::empty()).unwrap();
        let ctx = test_ctx();

        let file = fs.lookup(ctx, fuse::ROOT_ID, &cstr("file.txt")).unwrap();
        let (_, file_opts) = fs
            .open(ctx, file.inode, false, libc::O_RDONLY as u32)
            .unwrap();
        assert!(!file_opts.contains(fuse::OpenOptions::DIRECT_IO));
        assert!(!file_opts.contains(fuse::OpenOptions::KEEP_CACHE));
    }

    #[test]
    fn zero_timeouts_report_uncacheable_metadata() {
        let temp = TempRoot::new("metadata-timeouts");
        fs::write(temp.path().join("file.txt"), b"contents").unwrap();
        let fs = PassthroughFs::new(
            Config {
                root_dir: temp.path().to_string_lossy().into_owned(),
                attr_timeout: std::time::Duration::ZERO,
                entry_timeout: std::time::Duration::ZERO,
                ..Default::default()
            },
            Arc::new(InodeAllocator::new()),
        )
        .unwrap();
        fs.init(FsOptions::empty()).unwrap();
        let ctx = test_ctx();

        let entry = fs.lookup(ctx, fuse::ROOT_ID, &cstr("file.txt")).unwrap();
        let (_, attr_timeout) = fs.getattr(ctx, entry.inode, None).unwrap();

        assert_eq!(entry.attr_timeout, std::time::Duration::ZERO);
        assert_eq!(entry.entry_timeout, std::time::Duration::ZERO);
        assert_eq!(attr_timeout, std::time::Duration::ZERO);
    }

    #[test]
    fn setattr_mode_updates_host_file_mode() {
        let temp = TempRoot::new("chmod-host-mode");
        let path = temp.path().join("script.sh");
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let fs = new_fs(temp.path(), vec![PathBuf::from(".")]);
        let ctx = test_ctx();

        let file = fs.lookup(ctx, fuse::ROOT_ID, &cstr("script.sh")).unwrap();
        let (mut attr, _) = fs.getattr(ctx, file.inode, None).unwrap();
        attr.st_mode = 0o755;
        fs.setattr(ctx, file.inode, attr, None, SetattrValid::MODE)
            .unwrap();

        let host_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(host_mode, 0o755);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let (attr, _) = fs.getattr(ctx, file.inode, None).unwrap();
        assert_eq!(attr.st_mode as u32 & 0o777, 0o644);
    }

    #[test]
    fn restore_drops_dax_mapping_for_changed_host_file() {
        let temp = TempRoot::new("restore-dax-changed");
        let path = temp.path().join("file.txt");
        fs::write(&path, b"before\n").unwrap();
        let fs = new_fs(temp.path(), vec![PathBuf::from(".")]);
        let ctx = test_ctx();

        let file = fs.lookup(ctx, fuse::ROOT_ID, &cstr("file.txt")).unwrap();
        let mut snap = fs.snapshot_state().unwrap();
        snap.dax_mappings.push(DaxMappingSnapshot {
            guest_addr: 0x200000,
            inode: file.inode,
            foffset: 0,
            len: 2 * 1024 * 1024,
            writable: false,
        });

        fs::write(&path, b"after-after\n").unwrap();
        fs.restore_state(&snap).unwrap();

        assert!(fs.pending_dax_mappings.lock().unwrap().is_empty());
        let (st, _) = fs.getattr(ctx, file.inode, None).unwrap();
        assert_eq!(st.st_size, b"after-after\n".len() as i64);
    }

    #[test]
    fn restore_keeps_dax_mapping_for_unchanged_host_file() {
        let temp = TempRoot::new("restore-dax-unchanged");
        fs::write(temp.path().join("file.txt"), b"contents\n").unwrap();
        let fs = new_fs(temp.path(), vec![PathBuf::from(".")]);
        let ctx = test_ctx();

        let file = fs.lookup(ctx, fuse::ROOT_ID, &cstr("file.txt")).unwrap();
        let mut snap = fs.snapshot_state().unwrap();
        snap.dax_mappings.push(DaxMappingSnapshot {
            guest_addr: 0x200000,
            inode: file.inode,
            foffset: 0,
            len: 2 * 1024 * 1024,
            writable: false,
        });

        fs.restore_state(&snap).unwrap();

        assert_eq!(fs.pending_dax_mappings.lock().unwrap().len(), 1);
    }

    #[test]
    fn write_allowlist_rejects_sibling_creates() {
        let temp = TempRoot::new("policy");
        fs::create_dir(temp.path().join("allowed")).unwrap();
        fs::create_dir(temp.path().join("denied")).unwrap();
        let fs = new_fs(temp.path(), vec![PathBuf::from("allowed")]);
        let ctx = test_ctx();

        let allowed = fs.lookup(ctx, fuse::ROOT_ID, &cstr("allowed")).unwrap();
        fs.create(
            ctx,
            allowed.inode,
            &cstr("ok.txt"),
            0o644,
            false,
            libc::O_WRONLY as u32,
            0,
            Extensions::default(),
        )
        .unwrap();
        assert!(temp.path().join("allowed/ok.txt").exists());

        let denied = fs.lookup(ctx, fuse::ROOT_ID, &cstr("denied")).unwrap();
        let err = match fs.create(
            ctx,
            denied.inode,
            &cstr("no.txt"),
            0o644,
            false,
            libc::O_WRONLY as u32,
            0,
            Extensions::default(),
        ) {
            Ok(_) => panic!("denied create unexpectedly succeeded"),
            Err(err) => err,
        };
        assert_eq!(err.raw_os_error(), Some(libc::EROFS));
        assert!(!temp.path().join("denied/no.txt").exists());
    }

    #[test]
    fn gitignored_create_uses_upper_state() {
        let temp = TempRoot::new("gitignored-create");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        init_git_repo(&share, "ignored.txt\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        fs.create(
            ctx,
            fuse::ROOT_ID,
            &cstr("ignored.txt"),
            0o644,
            false,
            libc::O_WRONLY as u32,
            0,
            Extensions::default(),
        )
        .unwrap();

        assert!(!share.join("ignored.txt").exists());
        assert!(state.join("upper/ignored.txt").exists());
    }

    #[test]
    fn nonignored_create_writes_through_to_host() {
        let temp = TempRoot::new("tracked-create");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        init_git_repo(&share, "ignored.txt\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        fs.create(
            ctx,
            fuse::ROOT_ID,
            &cstr("tracked.txt"),
            0o644,
            false,
            libc::O_WRONLY as u32,
            0,
            Extensions::default(),
        )
        .unwrap();

        assert!(share.join("tracked.txt").exists());
        assert!(!state.join("upper/tracked.txt").exists());
    }

    #[test]
    fn gitignored_truncate_copies_up_before_mutating() {
        let temp = TempRoot::new("gitignored-truncate");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        fs::create_dir(share.join("state")).unwrap();
        fs::write(share.join("state/db.sqlite"), b"host contents").unwrap();
        init_git_repo(&share, "state/\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        let dir = fs.lookup(ctx, fuse::ROOT_ID, &cstr("state")).unwrap();
        let file = fs.lookup(ctx, dir.inode, &cstr("db.sqlite")).unwrap();
        fs.open(
            ctx,
            file.inode,
            false,
            libc::O_WRONLY as u32 | bindings::LINUX_O_TRUNC as u32,
        )
        .unwrap();

        assert_eq!(
            fs::read(share.join("state/db.sqlite")).unwrap(),
            b"host contents"
        );
        assert_eq!(
            fs::metadata(state.join("upper/state/db.sqlite"))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn gitignored_unlink_whiteouts_lower_file() {
        let temp = TempRoot::new("gitignored-whiteout");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        fs::create_dir(share.join("state")).unwrap();
        fs::write(share.join("state/db.sqlite"), b"host contents").unwrap();
        init_git_repo(&share, "state/\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        let dir = fs.lookup(ctx, fuse::ROOT_ID, &cstr("state")).unwrap();
        fs.lookup(ctx, dir.inode, &cstr("db.sqlite")).unwrap();
        fs.unlink(ctx, dir.inode, &cstr("db.sqlite")).unwrap();

        assert!(share.join("state/db.sqlite").exists());
        assert!(state.join("whiteouts/state/db.sqlite").exists());
        let err = match fs.lookup(ctx, dir.inode, &cstr("db.sqlite")) {
            Ok(_) => panic!("whiteout should hide lower file"),
            Err(err) => err,
        };
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn descendant_whiteout_directory_does_not_hide_ancestor() {
        let temp = TempRoot::new("descendant-whiteout");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        fs::create_dir(share.join("src")).unwrap();
        fs::create_dir(share.join("src/artifact-fs")).unwrap();
        fs::write(share.join("src/artifact-fs/main.go"), b"package main\n").unwrap();
        init_git_repo(&share, "src/artifact-fs/artifact-fs\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        fs::create_dir_all(state.join("whiteouts/src/artifact-fs")).unwrap();

        let src = fs.lookup(ctx, fuse::ROOT_ID, &cstr("src")).unwrap();
        let project = fs.lookup(ctx, src.inode, &cstr("artifact-fs")).unwrap();
        fs.lookup(ctx, project.inode, &cstr("main.go")).unwrap();
        let (handle, _) = fs
            .opendir(ctx, project.inode, libc::O_RDONLY as u32)
            .unwrap();
        let handle = handle.unwrap();
        let mut names = Vec::new();
        fs.readdir(ctx, project.inode, handle, 4096, 0, |entry| {
            names.push(String::from_utf8_lossy(entry.name).into_owned());
            Ok(1)
        })
        .unwrap();
        assert!(names.iter().any(|name| name == "main.go"));

        let entries = fs.overlay_dir_entries_from_paths(Path::new("src")).unwrap();
        assert!(entries.contains_key(b"artifact-fs".as_slice()));
    }

    #[test]
    fn direct_whiteout_can_coexist_with_descendant_whiteout_directory() {
        let temp = TempRoot::new("direct-descendant-whiteout");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        fs::create_dir(share.join("src")).unwrap();
        fs::create_dir(share.join("src/artifact-fs")).unwrap();
        init_git_repo(&share, "src/artifact-fs/\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        fs::create_dir_all(state.join("whiteouts/src/artifact-fs/nested")).unwrap();
        let src = fs.lookup(ctx, fuse::ROOT_ID, &cstr("src")).unwrap();
        fs.rmdir(ctx, src.inode, &cstr("artifact-fs")).unwrap();

        assert!(
            state
                .join("whiteouts/src/artifact-fs/.lnx-whiteout")
                .exists()
        );
        let err = match fs.lookup(ctx, src.inode, &cstr("artifact-fs")) {
            Ok(_) => panic!("direct whiteout should hide lower directory"),
            Err(err) => err,
        };
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn gitignored_rmdir_lower_file_returns_enotdir() {
        let temp = TempRoot::new("gitignored-rmdir-file");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        fs::create_dir(share.join("state")).unwrap();
        fs::write(share.join("state/db.sqlite"), b"host contents").unwrap();
        init_git_repo(&share, "state/\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        let dir = fs.lookup(ctx, fuse::ROOT_ID, &cstr("state")).unwrap();
        let err = match fs.rmdir(ctx, dir.inode, &cstr("db.sqlite")) {
            Ok(()) => panic!("rmdir on a lower file unexpectedly succeeded"),
            Err(err) => err,
        };

        assert_eq!(err.raw_os_error(), Some(libc::ENOTDIR));
        assert!(!state.join("whiteouts/state/db.sqlite").exists());
    }

    #[test]
    fn gitignored_rmdir_nonempty_lower_dir_returns_enotempty() {
        let temp = TempRoot::new("gitignored-rmdir-nonempty");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        fs::create_dir(share.join("state")).unwrap();
        fs::write(share.join("state/db.sqlite"), b"host contents").unwrap();
        init_git_repo(&share, "state/\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        let err = match fs.rmdir(ctx, fuse::ROOT_ID, &cstr("state")) {
            Ok(()) => panic!("rmdir on a non-empty lower directory unexpectedly succeeded"),
            Err(err) => err,
        };

        assert_eq!(err.raw_os_error(), Some(bindings::LINUX_ENOTEMPTY));
        assert!(share.join("state/db.sqlite").exists());
        assert!(!state.join("whiteouts/state").exists());
    }

    #[test]
    fn overlay_readdir_uses_open_lower_directory_fd() {
        let temp = TempRoot::new("overlay-readdir-fd");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        fs::create_dir(share.join("list")).unwrap();
        fs::write(share.join("list/file.txt"), b"contents").unwrap();
        init_git_repo(&share, "");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        let dir = fs.lookup(ctx, fuse::ROOT_ID, &cstr("list")).unwrap();
        let (handle, _) = fs.opendir(ctx, dir.inode, libc::O_RDONLY as u32).unwrap();
        let handle = handle.unwrap();
        fs::rename(share.join("list"), share.join("moved")).unwrap();

        let mut names = Vec::new();
        fs.readdir(ctx, dir.inode, handle, 4096, 0, |entry| {
            names.push(String::from_utf8_lossy(entry.name).into_owned());
            Ok(1)
        })
        .unwrap();

        assert!(names.iter().any(|name| name == "file.txt"));
    }

    #[test]
    fn overlay_rename_keeps_existing_inode_path() {
        let temp = TempRoot::new("overlay-rename-path");
        let share = temp.path().join("share");
        let state = temp.path().join("state");
        fs::create_dir(&share).unwrap();
        fs::create_dir(share.join("state")).unwrap();
        fs::write(share.join("state/db.sqlite"), b"host contents").unwrap();
        init_git_repo(&share, "state/\n");
        let fs = new_fs_with_unshare(&share, &state);
        let ctx = test_ctx();

        let dir = fs.lookup(ctx, fuse::ROOT_ID, &cstr("state")).unwrap();
        let file = fs.lookup(ctx, dir.inode, &cstr("db.sqlite")).unwrap();
        fs.rename(
            ctx,
            dir.inode,
            &cstr("db.sqlite"),
            dir.inode,
            &cstr("renamed.sqlite"),
            0,
        )
        .unwrap();

        let (mut attr, _) = fs.getattr(ctx, file.inode, None).unwrap();
        attr.st_mode = 0o600;
        fs.setattr(ctx, file.inode, attr, None, SetattrValid::MODE)
            .unwrap();

        let mode = fs::metadata(state.join("upper/state/renamed.sqlite"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn apfs_copyfilerange_preserves_holes_and_fallocate_punches_holes() {
        let temp = TempRoot::new("copy-range");
        let fs = new_fs(temp.path(), vec![PathBuf::from(".")]);
        let ctx = test_ctx();

        let src_path = temp.path().join("sparse-src.bin");
        let mut src = File::create(&src_path).unwrap();
        src.write_all(b"head").unwrap();
        src.seek(SeekFrom::Start(2 * 1024 * 1024)).unwrap();
        src.write_all(b"tail").unwrap();
        drop(src);

        let src_entry = fs
            .lookup(ctx, fuse::ROOT_ID, &cstr("sparse-src.bin"))
            .unwrap();
        let (src_handle, _) = fs
            .open(ctx, src_entry.inode, false, libc::O_RDONLY as u32)
            .unwrap();
        let src_handle = src_handle.unwrap();
        let (dst_entry, dst_handle, _) = fs
            .create(
                ctx,
                fuse::ROOT_ID,
                &cstr("sparse-dst.bin"),
                0o644,
                false,
                libc::O_WRONLY as u32,
                0,
                Extensions::default(),
            )
            .unwrap();
        let dst_handle = dst_handle.unwrap();

        let copied = fs
            .copyfilerange(
                ctx,
                src_entry.inode,
                src_handle,
                0,
                dst_entry.inode,
                dst_handle,
                0,
                2 * 1024 * 1024 + 4,
                0,
            )
            .unwrap();
        assert_eq!(copied, 2 * 1024 * 1024 + 4);

        let dst_path = temp.path().join("sparse-dst.bin");
        let mut dst = File::open(&dst_path).unwrap();
        let mut buf = [0u8; 4];
        dst.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"head");
        dst.seek(SeekFrom::Start(2 * 1024 * 1024)).unwrap();
        dst.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"tail");

        dst.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        let mut hole = [1u8; 4096];
        dst.read_exact(&mut hole).unwrap();
        assert_eq!(hole, [0u8; 4096]);

        let punch_path = temp.path().join("punch.bin");
        let mut punch = File::create(&punch_path).unwrap();
        punch.write_all(&vec![b'A'; 3 * 1024 * 1024]).unwrap();
        drop(punch);

        let punch_entry = fs.lookup(ctx, fuse::ROOT_ID, &cstr("punch.bin")).unwrap();
        let (punch_handle, _) = fs
            .open(ctx, punch_entry.inode, false, libc::O_RDWR as u32)
            .unwrap();
        fs.fallocate(
            ctx,
            punch_entry.inode,
            punch_handle.unwrap(),
            bindings::LINUX_FALLOC_FL_KEEP_SIZE as u32
                | bindings::LINUX_FALLOC_FL_PUNCH_HOLE as u32,
            1024 * 1024,
            1024 * 1024,
        )
        .unwrap();

        let mut punch = File::open(&punch_path).unwrap();
        punch.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        let mut zeros = [1u8; 4096];
        punch.read_exact(&mut zeros).unwrap();
        assert_eq!(zeros, [0u8; 4096]);
    }
}

struct HandleData {
    inode: Inode,
    file: RwLock<File>,
    dirstream: Mutex<DirStream>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PassthroughFsSnapshot {
    inodes: Vec<InodeDataSnapshot>,
    #[serde(default)]
    inode_paths: Vec<InodePathSnapshot>,
    #[serde(default)]
    dax_mappings: Vec<DaxMappingSnapshot>,
    next_inode: u64,
    handles: Vec<HandleDataSnapshot>,
    next_handle: u64,
    writeback: bool,
    announce_submounts: bool,
}

impl PassthroughFsSnapshot {
    pub(crate) fn inode_paths(&self) -> &[InodePathSnapshot] {
        &self.inode_paths
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct InodePathSnapshot {
    pub(crate) inode: Inode,
    pub(crate) path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InodeDataSnapshot {
    inode: Inode,
    ino: u64,
    dev: i32,
    refcount: u64,
    #[serde(default)]
    host_object: Option<HostObjectSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct HostObjectSnapshot {
    ino: u64,
    dev: i32,
    size: i64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
    mode: u32,
}

impl HostObjectSnapshot {
    fn from_stat(st: &bindings::stat64) -> Self {
        Self {
            ino: st.st_ino,
            dev: st.st_dev,
            size: st.st_size,
            mtime: st.st_mtime,
            mtime_nsec: st.st_mtime_nsec,
            ctime: st.st_ctime,
            ctime_nsec: st.st_ctime_nsec,
            mode: st.st_mode as u32,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HandleDataSnapshot {
    handle: Handle,
    inode: Inode,
    flags: i32,
    dirstream: DirStreamSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DirStreamSnapshot {
    entries: Vec<CachedDirEntrySnapshot>,
    ready: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedDirEntrySnapshot {
    ino: bindings::ino64_t,
    name: Vec<u8>,
    type_: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DaxMappingSnapshot {
    guest_addr: u64,
    inode: Inode,
    foffset: u64,
    len: u64,
    writable: bool,
}

#[derive(Clone, Debug)]
struct DaxMapping {
    host_addr: u64,
    snapshot: DaxMappingSnapshot,
}

fn ebadf() -> io::Error {
    linux_error(io::Error::from_raw_os_error(libc::EBADF))
}

fn einval() -> io::Error {
    linux_error(io::Error::from_raw_os_error(libc::EINVAL))
}

fn item_to_value(item: &[u8], radix: u32) -> Option<u32> {
    match std::str::from_utf8(item) {
        Ok(val) => match u32::from_str_radix(val, radix) {
            Ok(i) => Some(i),
            Err(e) => {
                debug!("invalid value: {radix} err={e}");
                None
            }
        },
        Err(_) => None,
    }
}

fn get_xattr_common(buf: &[u8]) -> io::Result<(Option<u32>, Option<u32>, Option<u32>)> {
    let mut items = buf.split(|c| *c == b':');

    let uid = match items.next() {
        Some(item) => item_to_value(item, 10),
        None => None,
    };
    let gid = match items.next() {
        Some(item) => item_to_value(item, 10),
        None => None,
    };
    let mode = match items.next() {
        Some(item) => item_to_value(item, 8),
        None => None,
    };

    Ok((uid, gid, mode))
}

fn get_xattr_fstat(
    fd: RawFd,
    st: bindings::stat64,
) -> io::Result<(Option<u32>, Option<u32>, Option<u32>)> {
    let mut buf: Vec<u8> = vec![0; 32];
    let options = if (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
        libc::XATTR_NOFOLLOW
    } else {
        0
    };
    let res = unsafe {
        libc::fgetxattr(
            fd,
            XATTR_KEY.as_ptr() as *const i8,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
            options,
        )
    };
    if res < 0 {
        debug!("fget_xattr error: {res}");
        return Ok((None, None, None));
    }

    buf.resize(res as usize, 0);

    get_xattr_common(&buf)
}

fn get_xattr_lstat(
    path: &CString,
    st: bindings::stat64,
) -> io::Result<(Option<u32>, Option<u32>, Option<u32>)> {
    let mut buf: Vec<u8> = vec![0; 32];
    let options = if (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
        libc::XATTR_NOFOLLOW
    } else {
        0
    };
    let res = unsafe {
        libc::getxattr(
            path.as_ptr(),
            XATTR_KEY.as_ptr() as *const i8,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
            options,
        )
    };
    if res < 0 {
        debug!("fget_xattr error: {res}");
        return Ok((None, None, None));
    }

    buf.resize(res as usize, 0);

    get_xattr_common(&buf)
}

fn is_valid_owner(owner: Option<(u32, u32)>) -> bool {
    if let Some(owner) = owner
        && owner.0 < UID_MAX
        && owner.1 < UID_MAX
    {
        return true;
    }

    false
}
// We won't need this once expressions like "if let ... &&" are allowed.
#[allow(clippy::unnecessary_unwrap)]
fn set_xattr_stat(
    ctx: &Context,
    file: &InodeHandle,
    st: Option<bindings::stat64>,
    owner: Option<(u32, u32)>,
    mode: Option<u32>,
) -> io::Result<()> {
    let st = st.unwrap_or(istat(ctx, file, true)?);
    let buf = if is_valid_owner(owner) && mode.is_some() {
        let owner = owner.unwrap();
        let mode = mode.unwrap();
        format!("{}:{}:0{:o}", owner.0, owner.1, mode)
    } else {
        let (orig_uid, orig_gid, orig_mode) = match file {
            InodeHandle::Fd(fd) => get_xattr_fstat(*fd, st)?,
            InodeHandle::Path(c_path) => get_xattr_lstat(c_path, st)?,
        };

        let (uid, gid) = match owner {
            Some(o) => {
                let uid = if o.0 < UID_MAX { Some(o.0) } else { orig_uid };
                let gid = if o.1 < UID_MAX { Some(o.1) } else { orig_gid };
                (uid, gid)
            }
            None => (orig_uid, orig_gid),
        };

        let mut buf = String::new();
        if let Some(uid) = uid {
            buf.push_str(&format!("{uid}"));
        } else {
            buf.push('x');
        }
        if let Some(gid) = gid {
            buf.push_str(&format!(":{gid}:"));
        } else {
            buf.push_str(":x:");
        }
        if let Some(mode) = mode {
            buf.push_str(&format!("0{:o}", mode));
        } else if let Some(orig_mode) = orig_mode {
            buf.push_str(&format!("0{:o}", orig_mode));
        } else {
            buf.push('x');
        }
        buf
    };

    write_xattr_stat(file, st, &buf)
}

fn clear_xattr_mode(
    ctx: &Context,
    file: &InodeHandle,
    st: Option<bindings::stat64>,
) -> io::Result<()> {
    let st = st.unwrap_or(istat(ctx, file, true)?);
    let (uid, gid, _) = match file {
        InodeHandle::Fd(fd) => get_xattr_fstat(*fd, st)?,
        InodeHandle::Path(c_path) => get_xattr_lstat(c_path, st)?,
    };
    let mut buf = String::new();
    if let Some(uid) = uid {
        buf.push_str(&format!("{uid}"));
    } else {
        buf.push('x');
    }
    if let Some(gid) = gid {
        buf.push_str(&format!(":{gid}:x"));
    } else {
        buf.push_str(":x:x");
    }
    write_xattr_stat(file, st, &buf)
}

fn write_xattr_stat(file: &InodeHandle, st: bindings::stat64, buf: &str) -> io::Result<()> {
    let options = if (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
        libc::XATTR_NOFOLLOW
    } else {
        0
    };
    let res = match file {
        InodeHandle::Path(path) => unsafe {
            libc::setxattr(
                path.as_ptr(),
                XATTR_KEY.as_ptr() as *const i8,
                buf.as_ptr() as *mut libc::c_void,
                buf.len() as libc::size_t,
                0,
                options,
            )
        },
        InodeHandle::Fd(fd) => unsafe {
            libc::fsetxattr(
                *fd,
                XATTR_KEY.as_ptr() as *const i8,
                buf.as_ptr() as *mut libc::c_void,
                buf.len() as libc::size_t,
                0,
                options,
            )
        },
    };

    if res < 0 {
        Err(linux_error(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn chmod_host(file: &InodeHandle, mode: u32) -> io::Result<()> {
    let mode = (mode & 0o7777) as libc::mode_t;
    let res = match file {
        InodeHandle::Path(path) => unsafe { libc::chmod(path.as_ptr(), mode) },
        InodeHandle::Fd(fd) => unsafe { libc::fchmod(*fd, mode) },
    };
    if res < 0 {
        Err(linux_error(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn stat_common(
    _ctx: &Context,
    mut st: bindings::stat64,
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<u32>,
) -> io::Result<bindings::stat64> {
    if let Some(uid) = uid {
        st.st_uid = uid;
    }
    if let Some(gid) = gid {
        st.st_gid = gid;
    }
    if let Some(mode) = mode {
        if mode as u16 & libc::S_IFMT == 0 {
            st.st_mode = (st.st_mode & libc::S_IFMT) | mode as u16;
        } else {
            st.st_mode = mode as u16;
        }
    }

    Ok(st)
}

fn fstat(ctx: &Context, fd: RawFd, host: bool) -> io::Result<bindings::stat64> {
    let mut st = MaybeUninit::<bindings::stat64>::zeroed();

    // Safe because the kernel will only write data in `st` and we check the return
    // value.
    let res = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
    if res >= 0 {
        // Safe because the kernel guarantees that the struct is now fully initialized.
        let st = unsafe { st.assume_init() };
        if !host {
            let (uid, gid, mode) = get_xattr_fstat(fd, st)?;
            stat_common(ctx, st, uid, gid, mode)
        } else {
            Ok(st)
        }
    } else {
        Err(linux_error(io::Error::last_os_error()))
    }
}

fn punch_hole(fd: RawFd, offset: u64, length: u64) -> io::Result<()> {
    if length == 0 {
        return Ok(());
    }

    let mut hole = libc::fpunchhole_t {
        fp_offset: offset as i64,
        fp_flags: 0,
        reserved: 0,
        fp_length: length as i64,
    };
    let res = unsafe { libc::fcntl(fd, libc::F_PUNCHHOLE, &mut hole as *mut _) };
    if res < 0 {
        return Err(linux_error(io::Error::last_os_error()));
    }
    Ok(())
}

fn lstat(ctx: &Context, c_path: &CString, host: bool) -> io::Result<bindings::stat64> {
    let mut st = MaybeUninit::<bindings::stat64>::zeroed();

    // Safe because the kernel will only write data in `st` and we check the return
    // value.
    let res = unsafe { libc::lstat(c_path.as_ptr(), st.as_mut_ptr()) };
    if res >= 0 {
        // Safe because the kernel guarantees that the struct is now fully initialized.
        let st = unsafe { st.assume_init() };
        if !host {
            let (uid, gid, mode) = get_xattr_lstat(c_path, st)?;
            stat_common(ctx, st, uid, gid, mode)
        } else {
            Ok(st)
        }
    } else {
        Err(linux_error(io::Error::last_os_error()))
    }
}

fn istat(ctx: &Context, ihandle: &InodeHandle, host: bool) -> io::Result<bindings::stat64> {
    match ihandle {
        InodeHandle::Fd(fd) => fstat(ctx, *fd, host),
        InodeHandle::Path(c_path) => lstat(ctx, c_path, host),
    }
}

/// The caching policy that the file system should report to the FUSE client. By default the FUSE
/// protocol uses close-to-open consistency. This means that any cached contents of the file are
/// invalidated the next time that file is opened.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum CachePolicy {
    /// The client should never cache file data and all I/O should be directly forwarded to the
    /// server. This policy must be selected when file contents may change without the knowledge of
    /// the FUSE client (i.e., the file system does not have exclusive access to the directory).
    Never,

    /// The client is free to choose when and how to cache file data. This is the default policy and
    /// uses close-to-open consistency as described in the enum documentation.
    #[default]
    Auto,

    /// The client should always cache file data. This means that the FUSE client will not
    /// invalidate any cached data that was returned by the file system the last time the file was
    /// opened. This policy should only be selected when the file system has exclusive access to the
    /// directory.
    Always,
}

impl FromStr for CachePolicy {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "never" | "Never" | "NEVER" => Ok(CachePolicy::Never),
            "auto" | "Auto" | "AUTO" => Ok(CachePolicy::Auto),
            "always" | "Always" | "ALWAYS" => Ok(CachePolicy::Always),
            _ => Err("invalid cache policy"),
        }
    }
}

/// Options that configure the behavior of the file system.
#[derive(Debug, Clone)]
pub struct Config {
    /// How long the FUSE client should consider directory entries to be valid. If the contents of a
    /// directory can only be modified by the FUSE client (i.e., the file system has exclusive
    /// access), then this should be a large value.
    ///
    /// The default value for this option is 5 seconds.
    pub entry_timeout: Duration,

    /// How long the FUSE client should consider file and directory attributes to be valid. If the
    /// attributes of a file or directory can only be modified by the FUSE client (i.e., the file
    /// system has exclusive access), then this should be set to a large value.
    ///
    /// The default value for this option is 5 seconds.
    pub attr_timeout: Duration,

    /// The caching policy the file system should use. See the documentation of `CachePolicy` for
    /// more details.
    pub cache_policy: CachePolicy,

    /// Whether the file system should enabled writeback caching. This can improve performance as it
    /// allows the FUSE client to cache and coalesce multiple writes before sending them to the file
    /// system. However, enabling this option can increase the risk of data corruption if the file
    /// contents can change without the knowledge of the FUSE client (i.e., the server does **NOT**
    /// have exclusive access). Additionally, the file system should have read access to all files
    /// in the directory it is serving as the FUSE client may send read requests even for files
    /// opened with `O_WRONLY`.
    ///
    /// Therefore callers should only enable this option when they can guarantee that: 1) the file
    /// system has exclusive access to the directory and 2) the file system has read permissions for
    /// all files in that directory.
    ///
    /// The default value for this option is `false`.
    pub writeback: bool,

    /// The path of the root directory.
    ///
    /// The default is `/`.
    pub root_dir: String,

    /// Whether the file system should support Extended Attributes (xattr). Enabling this feature may
    /// have a significant impact on performance, especially on write parallelism. This is the result
    /// of FUSE attempting to remove the special file privileges after each write request.
    ///
    /// The default value for this options is `false`.
    pub xattr: bool,

    /// Optional file descriptor for /proc/self/fd. Callers can obtain a file descriptor and pass it
    /// here, so there's no need to open it in PassthroughFs::new(). This is specially useful for
    /// sandboxing.
    ///
    /// The default is `None`.
    pub proc_sfd_rawfd: Option<RawFd>,

    /// ID of this filesystem to uniquely identify exports. Not supported for macos.
    pub export_fsid: u64,
    /// Table of exported FDs to share with other subsystems. Not supported for macos.
    pub export_table: Option<ExportTable>,
    pub write_allowlist: Option<Arc<RwLock<Vec<PathBuf>>>>,
    pub unshare_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            entry_timeout: Duration::from_secs(5),
            attr_timeout: Duration::from_secs(5),
            cache_policy: Default::default(),
            writeback: false,
            root_dir: String::from("/"),
            xattr: true,
            proc_sfd_rawfd: None,
            export_fsid: 0,
            export_table: None,
            write_allowlist: None,
            unshare_dir: None,
        }
    }
}

/// A file system that simply "passes through" all requests it receives to the underlying file
/// system. To keep the implementation simple it servers the contents of its root directory. Users
/// that wish to serve only a specific directory should set up the environment so that that
/// directory ends up as the root of the file system process. One way to accomplish this is via a
/// combination of mount namespaces and the pivot_root system call.
pub struct PassthroughFs {
    inodes: RwLock<MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>>,
    inode_alloc: Arc<InodeAllocator>,

    handles: RwLock<BTreeMap<Handle, Arc<HandleData>>>,
    inode_paths: RwLock<BTreeMap<Inode, PathBuf>>,
    next_handle: AtomicU64,

    map_windows: Mutex<HashMap<u64, DaxMapping>>,
    pending_dax_mappings: Mutex<Vec<DaxMappingSnapshot>>,

    // Whether writeback caching is enabled for this directory. This will only be true when
    // `cfg.writeback` is true and `init` was called with `FsOptions::WRITEBACK_CACHE`.
    writeback: AtomicBool,
    announce_submounts: AtomicBool,
    cfg: Config,
}

impl PassthroughFs {
    const WHITEOUT_MARKER: &'static str = ".lnx-whiteout";

    pub fn new(cfg: Config, inode_alloc: Arc<InodeAllocator>) -> io::Result<PassthroughFs> {
        let root = CString::new(cfg.root_dir.as_str()).expect("CString::new failed");

        // Safe because this doesn't modify any memory and we check the return value.
        let fd = unsafe {
            libc::openat(
                libc::AT_FDCWD,
                root.as_ptr(),
                libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }

        unsafe { libc::close(fd) };

        if let Some(unshare_dir) = &cfg.unshare_dir {
            fs::create_dir_all(unshare_dir.join("upper")).map_err(linux_error)?;
            fs::create_dir_all(unshare_dir.join("whiteouts")).map_err(linux_error)?;
        }

        Ok(PassthroughFs {
            inodes: RwLock::new(MultikeyBTreeMap::new()),
            inode_alloc,

            handles: RwLock::new(BTreeMap::new()),
            inode_paths: RwLock::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),

            map_windows: Mutex::new(HashMap::new()),
            pending_dax_mappings: Mutex::new(Vec::new()),

            writeback: AtomicBool::new(false),
            announce_submounts: AtomicBool::new(false),
            cfg,
        })
    }

    fn rel_child_path(&self, parent: Inode, name: &CStr) -> io::Result<PathBuf> {
        let parent_path = self
            .inode_paths
            .read()
            .unwrap()
            .get(&parent)
            .cloned()
            .ok_or_else(ebadf)?;
        let name = name.to_str().map_err(|_| einval())?;
        if name.contains('/') || name == "." || name == ".." {
            return Err(einval());
        }
        Ok(parent_path.join(name))
    }

    fn remember_inode_path(&self, inode: Inode, path: PathBuf) {
        self.inode_paths.write().unwrap().insert(inode, path);
    }

    fn move_inode_path(&self, old_path: &Path, new_path: &Path) -> Vec<Inode> {
        let mut inode_paths = self.inode_paths.write().unwrap();
        let moved: Vec<Inode> = inode_paths
            .iter()
            .filter_map(|(&inode, path)| (path == old_path).then_some(inode))
            .collect();

        if moved.is_empty() {
            return moved;
        }

        let moved_set: HashSet<Inode> = moved.iter().copied().collect();
        inode_paths.retain(|inode, path| moved_set.contains(inode) || path != new_path);
        for inode in &moved {
            inode_paths.insert(*inode, new_path.to_path_buf());
        }

        moved
    }

    fn inode_path(&self, inode: Inode) -> io::Result<PathBuf> {
        self.inode_paths
            .read()
            .unwrap()
            .get(&inode)
            .cloned()
            .ok_or_else(ebadf)
    }

    fn write_allowed_path(&self, path: &PathBuf) -> bool {
        let Some(allowlist) = &self.cfg.write_allowlist else {
            return true;
        };
        allowlist.read().unwrap().iter().any(|allowed| {
            allowed.as_os_str() == "." || path == allowed || path.starts_with(allowed)
        })
    }

    fn check_write_path(&self, path: &PathBuf) -> io::Result<()> {
        if self.write_allowed_path(path) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::EROFS))
        }
    }

    fn check_write_inode(&self, inode: Inode) -> io::Result<()> {
        self.check_write_path(&self.inode_path(inode)?)
    }

    fn check_write_child(&self, parent: Inode, name: &CStr) -> io::Result<PathBuf> {
        let path = self.rel_child_path(parent, name)?;
        self.check_write_path(&path)?;
        Ok(path)
    }

    fn copy_sparse_range(
        &self,
        ctx: &Context,
        fd_in: RawFd,
        offset_in: u64,
        fd_out: RawFd,
        offset_out: u64,
        len: u64,
    ) -> io::Result<usize> {
        let src_size = fstat(ctx, fd_in, true)?.st_size.max(0) as u64;
        if offset_in >= src_size || len == 0 {
            return Ok(0);
        }

        let end = offset_in.saturating_add(len).min(src_size);
        let dst_end = offset_out + (end - offset_in);
        if (fstat(ctx, fd_out, true)?.st_size.max(0) as u64) < dst_end {
            let res = unsafe { libc::ftruncate(fd_out, dst_end as libc::off_t) };
            if res < 0 {
                return Err(linux_error(io::Error::last_os_error()));
            }
        }

        let mut pos = offset_in;
        let mut copied = 0usize;
        let mut buf = vec![0u8; 1024 * 1024];

        while pos < end {
            let data = unsafe { libc::lseek(fd_in, pos as libc::off_t, libc::SEEK_DATA) };
            if data < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ENXIO) {
                    break;
                }
                return Err(linux_error(err));
            }

            let data = data as u64;
            if data >= end {
                break;
            }

            if data > pos {
                punch_hole(fd_out, offset_out + (pos - offset_in), data - pos)?;
            }

            let hole = unsafe { libc::lseek(fd_in, data as libc::off_t, libc::SEEK_HOLE) };
            if hole < 0 {
                return Err(linux_error(io::Error::last_os_error()));
            }

            let mut data_pos = data;
            let data_end = (hole as u64).min(end);
            while data_pos < data_end {
                let chunk = (data_end - data_pos).min(buf.len() as u64) as usize;
                let read = unsafe {
                    libc::pread(
                        fd_in,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        chunk,
                        data_pos as libc::off_t,
                    )
                };
                if read < 0 {
                    return Err(linux_error(io::Error::last_os_error()));
                }
                if read == 0 {
                    break;
                }

                let read = read as usize;
                let mut written = 0usize;
                while written < read {
                    let out_pos = offset_out + (data_pos - offset_in) + written as u64;
                    let wrote = unsafe {
                        libc::pwrite(
                            fd_out,
                            buf[written..read].as_ptr() as *const libc::c_void,
                            read - written,
                            out_pos as libc::off_t,
                        )
                    };
                    if wrote < 0 {
                        return Err(linux_error(io::Error::last_os_error()));
                    }
                    if wrote == 0 {
                        return Err(linux_error(io::Error::from_raw_os_error(libc::EIO)));
                    }
                    written += wrote as usize;
                }

                data_pos += read as u64;
                copied = copied.saturating_add(read);
            }

            pos = data_end;
        }

        if pos < end {
            punch_hole(fd_out, offset_out + (pos - offset_in), end - pos)?;
        }

        Ok((end - offset_in).try_into().unwrap_or(copied))
    }

    fn rel_path_to_cstring(&self, rel_path: &Path) -> io::Result<CString> {
        let mut path = PathBuf::from(&self.cfg.root_dir);
        if !rel_path.as_os_str().is_empty() {
            path.push(rel_path);
        }
        CString::new(path.to_string_lossy().into_owned()).map_err(|_| einval())
    }

    fn path_to_cstring(&self, path: &Path) -> io::Result<CString> {
        CString::new(path.to_string_lossy().into_owned()).map_err(|_| einval())
    }

    fn unshare_dir(&self) -> Option<&Path> {
        self.cfg.unshare_dir.as_deref()
    }

    fn upper_root(&self) -> Option<PathBuf> {
        self.unshare_dir().map(|path| path.join("upper"))
    }

    fn whiteout_root(&self) -> Option<PathBuf> {
        self.unshare_dir().map(|path| path.join("whiteouts"))
    }

    fn lower_path(&self, rel_path: &Path) -> PathBuf {
        let mut path = PathBuf::from(&self.cfg.root_dir);
        if !rel_path.as_os_str().is_empty() {
            path.push(rel_path);
        }
        path
    }

    fn upper_path(&self, rel_path: &Path) -> Option<PathBuf> {
        let mut path = self.upper_root()?;
        if !rel_path.as_os_str().is_empty() {
            path.push(rel_path);
        }
        Some(path)
    }

    fn whiteout_path(&self, rel_path: &Path) -> Option<PathBuf> {
        let mut path = self.whiteout_root()?;
        if !rel_path.as_os_str().is_empty() {
            path.push(rel_path);
        }
        Some(path)
    }

    fn has_path(path: &Path) -> io::Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(linux_error(err)),
        }
    }

    fn path_metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(linux_error(err)),
        }
    }

    fn direct_whiteout_exists(&self, rel_path: &Path) -> io::Result<bool> {
        let Some(path) = self.whiteout_path(rel_path) else {
            return Ok(false);
        };
        match Self::path_metadata(&path)? {
            Some(metadata) if metadata.file_type().is_dir() => {
                Self::has_path(&path.join(Self::WHITEOUT_MARKER))
            }
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }

    fn whiteout_covers(&self, rel_path: &Path) -> io::Result<bool> {
        if self.unshare_dir().is_none() {
            return Ok(false);
        }
        for ancestor in rel_path.ancestors() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            if self.direct_whiteout_exists(ancestor)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn clear_direct_whiteout(&self, rel_path: &Path) -> io::Result<()> {
        let Some(path) = self.whiteout_path(rel_path) else {
            return Ok(());
        };
        if Self::path_metadata(&path)?.is_some_and(|metadata| metadata.file_type().is_dir()) {
            match fs::remove_file(path.join(Self::WHITEOUT_MARKER)) {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(linux_error(err)),
            }
        }
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(linux_error(err)),
        }
    }

    fn create_direct_whiteout(&self, rel_path: &Path) -> io::Result<()> {
        let Some(path) = self.whiteout_path(rel_path) else {
            return Ok(());
        };
        if Self::path_metadata(&path)?.is_some_and(|metadata| metadata.file_type().is_dir()) {
            fs::write(path.join(Self::WHITEOUT_MARKER), b"whiteout\n").map_err(linux_error)?;
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(linux_error)?;
        }
        fs::write(path, b"whiteout\n").map_err(linux_error)
    }

    fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
        let mut current = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()?.to_path_buf()
        };
        loop {
            if current.exists() {
                return Some(current);
            }
            if !current.pop() {
                return None;
            }
        }
    }

    fn git_ignored(&self, rel_path: &Path) -> bool {
        if self.unshare_dir().is_none() || rel_path.as_os_str().is_empty() {
            return false;
        }
        let lower = self.lower_path(rel_path);
        let Some(cwd) = Self::nearest_existing_ancestor(&lower) else {
            return false;
        };
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .arg("check-ignore")
            .arg("-q")
            .arg("--")
            .arg(&lower)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn path_has_upper(&self, rel_path: &Path) -> io::Result<bool> {
        let Some(path) = self.upper_path(rel_path) else {
            return Ok(false);
        };
        Self::has_path(&path)
    }

    fn should_unshare_path(&self, rel_path: &Path) -> io::Result<bool> {
        if self.unshare_dir().is_none() || rel_path.as_os_str().is_empty() {
            return Ok(false);
        }
        Ok(self.path_has_upper(rel_path)?
            || self.whiteout_covers(rel_path)?
            || self.git_ignored(rel_path))
    }

    fn overlay_path(
        &self,
        _parent: Inode,
        name: &CStr,
        rel_path: &Path,
    ) -> io::Result<OverlayPath> {
        if self.whiteout_covers(rel_path)? {
            return Ok(OverlayPath::Whiteout);
        }
        if let Some(upper) = self.upper_path(rel_path)
            && Self::has_path(&upper)?
        {
            let upper_metadata = Self::path_metadata(&upper)?;
            let lower = self.lower_path(rel_path);
            if upper_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.file_type().is_dir())
                && Self::path_metadata(&lower)?
                    .as_ref()
                    .is_some_and(|metadata| metadata.file_type().is_dir())
            {
                return Ok(OverlayPath::Lower(self.rel_path_to_cstring(rel_path)?));
            }
            return Ok(OverlayPath::Upper(self.path_to_cstring(&upper)?));
        }
        let _ = name;
        Ok(OverlayPath::Lower(self.rel_path_to_cstring(rel_path)?))
    }

    fn prepare_upper_parent(&self, ctx: &Context, rel_path: &Path) -> io::Result<()> {
        let Some(parent) = rel_path.parent() else {
            return Ok(());
        };
        let Some(upper_root) = self.upper_root() else {
            return Ok(());
        };
        fs::create_dir_all(&upper_root).map_err(linux_error)?;

        let mut current = PathBuf::new();
        for component in parent.components() {
            current.push(component.as_os_str());
            let Some(upper) = self.upper_path(&current) else {
                continue;
            };
            if Self::has_path(&upper)? {
                continue;
            }
            let lower = self.lower_path(&current);
            fs::create_dir(&upper).map_err(linux_error)?;
            if Self::has_path(&lower)? {
                let lower_c = self.path_to_cstring(&lower)?;
                let upper_c = self.path_to_cstring(&upper)?;
                let st = lstat(ctx, &lower_c, false)?;
                self.mirror_copied_metadata(ctx, &InodeHandle::Path(upper_c), st)?;
            }
        }
        Ok(())
    }

    fn prepare_upper_create(&self, ctx: &Context, rel_path: &Path) -> io::Result<CString> {
        self.prepare_upper_parent(ctx, rel_path)?;
        self.clear_direct_whiteout(rel_path)?;
        let upper = self.upper_path(rel_path).ok_or_else(einval)?;
        self.path_to_cstring(&upper)
    }

    fn mirror_copied_metadata(
        &self,
        ctx: &Context,
        file: &InodeHandle,
        st: bindings::stat64,
    ) -> io::Result<()> {
        if (st.st_mode & libc::S_IFMT) != libc::S_IFLNK {
            chmod_host(file, st.st_mode as u32)?;
        }
        set_xattr_stat(
            ctx,
            file,
            None,
            Some((st.st_uid, st.st_gid)),
            Some(st.st_mode as u32),
        )
    }

    fn copy_up_path(&self, ctx: &Context, rel_path: &Path) -> io::Result<()> {
        if self.path_has_upper(rel_path)? {
            return Ok(());
        }
        if self.whiteout_covers(rel_path)? && !self.direct_whiteout_exists(rel_path)? {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOENT)));
        }

        let lower = self.lower_path(rel_path);
        let upper = self.upper_path(rel_path).ok_or_else(einval)?;
        let lower_c = self.path_to_cstring(&lower)?;
        let st = lstat(ctx, &lower_c, false)?;
        self.prepare_upper_parent(ctx, rel_path)?;

        match st.st_mode & libc::S_IFMT {
            mode if mode == libc::S_IFDIR => {
                fs::create_dir(&upper).map_err(linux_error)?;
            }
            mode if mode == libc::S_IFLNK => {
                let target = fs::read_link(&lower).map_err(linux_error)?;
                std::os::unix::fs::symlink(target, &upper).map_err(linux_error)?;
            }
            mode if mode == libc::S_IFREG => {
                fs::copy(&lower, &upper).map_err(linux_error)?;
            }
            _ => {
                return Err(linux_error(io::Error::from_raw_os_error(libc::EOPNOTSUPP)));
            }
        }

        let upper_c = self.path_to_cstring(&upper)?;
        self.mirror_copied_metadata(ctx, &InodeHandle::Path(upper_c), st)?;
        self.clear_direct_whiteout(rel_path)
    }

    fn refresh_inode_backing(
        &self,
        ctx: &Context,
        inode: Inode,
        rel_path: &Path,
    ) -> io::Result<()> {
        let Some(upper) = self.upper_path(rel_path) else {
            return Ok(());
        };
        let upper_c = self.path_to_cstring(&upper)?;
        let st = lstat(ctx, &upper_c, false)?;
        let old = self.inodes.read().unwrap().get(&inode).cloned();
        let refcount = old
            .as_ref()
            .map(|data| data.refcount.load(Ordering::Acquire))
            .unwrap_or(1);
        self.inodes.write().unwrap().insert(
            inode,
            InodeAltKey {
                ino: st.st_ino,
                dev: st.st_dev,
            },
            Arc::new(InodeData {
                inode,
                ino: st.st_ino,
                dev: st.st_dev,
                refcount: AtomicU64::new(refcount),
                unlinked_fd: AtomicI64::new(-1),
            }),
        );
        Ok(())
    }

    fn ensure_unshared_inode(&self, ctx: &Context, inode: Inode) -> io::Result<bool> {
        let rel_path = self.inode_path(inode)?;
        if !self.should_unshare_path(&rel_path)? {
            return Ok(false);
        }
        self.copy_up_path(ctx, &rel_path)?;
        self.refresh_inode_backing(ctx, inode, &rel_path)?;
        Ok(true)
    }

    fn child_write_path(
        &self,
        ctx: &Context,
        parent: Inode,
        name: &CStr,
        rel_path: &Path,
    ) -> io::Result<CString> {
        if self.should_unshare_path(rel_path)? {
            self.prepare_upper_create(ctx, rel_path)
        } else {
            self.name_to_path(parent, name)
        }
    }

    fn current_host_object(
        &self,
        rel_path: &Path,
    ) -> io::Result<(InodeAltKey, HostObjectSnapshot)> {
        let path = if self.whiteout_covers(rel_path)? {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOENT)));
        } else if let Some(upper) = self.upper_path(rel_path) {
            if Self::has_path(&upper)? {
                self.path_to_cstring(&upper)?
            } else {
                self.rel_path_to_cstring(rel_path)?
            }
        } else {
            self.rel_path_to_cstring(rel_path)?
        };
        let ctx = Context {
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let st = lstat(&ctx, &path, false)?;
        Ok((
            InodeAltKey {
                ino: st.st_ino,
                dev: st.st_dev,
            },
            HostObjectSnapshot::from_stat(&st),
        ))
    }

    fn snapshot_host_object(&self, data: &InodeData) -> Option<HostObjectSnapshot> {
        let path = CString::new(format!("/.vol/{}/{}", data.dev, data.ino)).ok()?;
        let ctx = Context {
            uid: 0,
            gid: 0,
            pid: 0,
        };
        lstat(&ctx, &path, false)
            .ok()
            .map(|st| HostObjectSnapshot::from_stat(&st))
    }

    fn install_dax_mapping(
        &self,
        mapping: DaxMappingSnapshot,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        if map_sender.is_none() {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOSYS)));
        }

        let open_flags = if mapping.writable {
            libc::O_RDWR
        } else {
            libc::O_RDONLY
        };
        // HVF rejects read-only file mappings as guest memory. For FUSE
        // read-only mappings, use a private writable host mapping while the
        // guest still gets the access mode requested by the kernel.
        let prot_flags = libc::PROT_READ | libc::PROT_WRITE;
        let mmap_flags = if mapping.writable {
            libc::MAP_SHARED
        } else {
            libc::MAP_PRIVATE
        };
        let hv_protection = if mapping.writable { 1 | 2 } else { 1 | 2 | 4 };

        let file = self.open_inode(mapping.inode, open_flags)?;
        let fd = file.as_raw_fd();
        let host_addr = unsafe {
            mmap_dax_aligned(
                mapping.len as usize,
                DAX_MAPPING_ALIGNMENT,
                prot_flags,
                mmap_flags,
                fd,
                mapping.foffset as libc::off_t,
            )
        };
        if host_addr == libc::MAP_FAILED {
            return Err(linux_error(io::Error::last_os_error()));
        }

        drop(file);

        let sender = map_sender.as_ref().unwrap();
        let (reply_sender, reply_receiver) = unbounded();
        sender
            .send(WorkerMessage::GpuAddMapping(
                reply_sender,
                host_addr as u64,
                mapping.guest_addr,
                mapping.len,
                hv_protection,
            ))
            .unwrap();
        if !reply_receiver.recv().unwrap() {
            error!("Error requesting HVF the addition of a DAX window");
            unsafe { libc::munmap(host_addr, mapping.len as usize) };
            return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
        }

        self.map_windows.lock().unwrap().insert(
            mapping.guest_addr,
            DaxMapping {
                host_addr: host_addr as u64,
                snapshot: mapping,
            },
        );

        Ok(())
    }

    pub(crate) fn snapshot_state(&self) -> io::Result<PassthroughFsSnapshot> {
        let inodes = self
            .inodes
            .read()
            .unwrap()
            .iter()
            .map(|(_, data)| {
                let unlinked_fd = data.unlinked_fd.load(Ordering::Acquire);
                if unlinked_fd >= 0 {
                    return Err(linux_error(io::Error::from_raw_os_error(libc::ENOTSUP)));
                }
                Ok(InodeDataSnapshot {
                    inode: data.inode,
                    ino: data.ino,
                    dev: data.dev,
                    refcount: data.refcount.load(Ordering::Acquire),
                    host_object: self.snapshot_host_object(data),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;

        let handles = self
            .handles
            .read()
            .unwrap()
            .iter()
            .map(|(&handle, data)| {
                let file = data.file.read().unwrap();
                let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
                if flags < 0 {
                    return Err(linux_error(io::Error::last_os_error()));
                }
                let dirstream = data.dirstream.lock().unwrap();
                Ok(HandleDataSnapshot {
                    handle,
                    inode: data.inode,
                    flags,
                    dirstream: DirStreamSnapshot {
                        ready: dirstream.ready,
                        entries: dirstream
                            .entries
                            .iter()
                            .map(|e| CachedDirEntrySnapshot {
                                ino: e.ino,
                                name: e.name.to_vec(),
                                type_: e.type_,
                            })
                            .collect(),
                    },
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let inode_paths = self
            .inode_paths
            .read()
            .unwrap()
            .iter()
            .map(|(&inode, path)| InodePathSnapshot {
                inode,
                path: path.to_string_lossy().into_owned(),
            })
            .collect();
        let dax_mappings = self
            .map_windows
            .lock()
            .unwrap()
            .values()
            .map(|mapping| mapping.snapshot.clone())
            .collect();

        Ok(PassthroughFsSnapshot {
            inodes,
            inode_paths,
            dax_mappings,
            next_inode: self.inode_alloc.snapshot_next(),
            handles,
            next_handle: self.next_handle.load(Ordering::Acquire),
            writeback: self.writeback.load(Ordering::Acquire),
            announce_submounts: self.announce_submounts.load(Ordering::Acquire),
        })
    }

    pub(crate) fn restore_state(&self, snap: &PassthroughFsSnapshot) -> io::Result<()> {
        let host_share = self.cfg.write_allowlist.is_some();
        let path_by_inode = snap
            .inode_paths
            .iter()
            .map(|path| (path.inode, PathBuf::from(&path.path)))
            .collect::<HashMap<_, _>>();
        let mut changed_inodes = HashSet::new();
        let mut inodes = MultikeyBTreeMap::new();
        for inode in &snap.inodes {
            let mut ino = inode.ino;
            let mut dev = inode.dev;
            if host_share {
                let current = path_by_inode
                    .get(&inode.inode)
                    .map(|path| self.current_host_object(path));
                match current {
                    Some(Ok((altkey, object))) => {
                        ino = altkey.ino;
                        dev = altkey.dev;
                        if inode.host_object.as_ref() != Some(&object) {
                            changed_inodes.insert(inode.inode);
                        }
                    }
                    Some(Err(_)) | None => {
                        changed_inodes.insert(inode.inode);
                    }
                }
            }
            inodes.insert(
                inode.inode,
                InodeAltKey { ino, dev },
                Arc::new(InodeData {
                    inode: inode.inode,
                    ino,
                    dev,
                    refcount: AtomicU64::new(inode.refcount),
                    unlinked_fd: AtomicI64::new(-1),
                }),
            );
        }

        let mut handles = BTreeMap::new();
        {
            let mut current = self.inodes.write().unwrap();
            *current = inodes;
        }
        *self.inode_paths.write().unwrap() = snap
            .inode_paths
            .iter()
            .map(|path| (path.inode, PathBuf::from(&path.path)))
            .collect();
        *self.map_windows.lock().unwrap() = HashMap::new();
        *self.pending_dax_mappings.lock().unwrap() = if host_share {
            snap.dax_mappings
                .iter()
                .filter(|mapping| !changed_inodes.contains(&mapping.inode))
                .cloned()
                .collect()
        } else {
            snap.dax_mappings.clone()
        };
        for handle in &snap.handles {
            let flags = handle.flags & !libc::O_EXLOCK;
            let backing = self
                .inodes
                .read()
                .unwrap()
                .get(&handle.inode)
                .map(|inode| format!("/.vol/{}/{}", inode.dev, inode.ino))
                .unwrap_or_else(|| "<missing inode>".to_string());
            let file = match self.open_inode(handle.inode, flags) {
                Ok(file) => RwLock::new(file),
                Err(e) if host_share && changed_inodes.contains(&handle.inode) => {
                    warn!(
                        "dropping restored virtio-fs handle {} inode {} backing {} flags {:#x}: {}",
                        handle.handle, handle.inode, backing, flags, e
                    );
                    continue;
                }
                Err(e) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                            "open restored handle {} inode {} backing {} flags {:#x}: {}",
                            handle.handle, handle.inode, backing, flags, e
                        ),
                    ));
                }
            };
            let dirstream = if host_share {
                DirStream::new()
            } else {
                DirStream {
                    ready: handle.dirstream.ready,
                    entries: handle
                        .dirstream
                        .entries
                        .iter()
                        .map(|e| CachedDirEntry {
                            ino: e.ino,
                            name: Box::<[u8]>::from(e.name.as_slice()),
                            type_: e.type_,
                        })
                        .collect(),
                }
            };
            handles.insert(
                handle.handle,
                Arc::new(HandleData {
                    inode: handle.inode,
                    file,
                    dirstream: Mutex::new(dirstream),
                }),
            );
        }

        *self.handles.write().unwrap() = handles;
        self.inode_alloc.restore_next(snap.next_inode);
        self.next_handle.store(snap.next_handle, Ordering::Release);
        self.writeback.store(snap.writeback, Ordering::Release);
        self.announce_submounts
            .store(snap.announce_submounts, Ordering::Release);
        Ok(())
    }

    pub(crate) fn replay_dax_mappings(
        &self,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        let mappings = {
            let mut pending = self.pending_dax_mappings.lock().unwrap();
            if pending.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *pending)
        };

        for mapping in mappings {
            self.install_dax_mapping(mapping, map_sender)?;
        }
        Ok(())
    }

    fn inode_to_handle(&self, inode: Inode, supports_fd: bool) -> io::Result<InodeHandle> {
        debug!("inode_to_handle: inode={inode}");
        let data = self
            .inodes
            .read()
            .unwrap()
            .get(&inode)
            .cloned()
            .ok_or_else(ebadf)?;

        let cstr =
            CString::new(format!("/.vol/{}/{}", data.dev, data.ino)).map_err(|_| einval())?;
        debug!("inode_to_handle: path={}", cstr.to_string_lossy());

        if supports_fd {
            let unlinked_fd = data.unlinked_fd.load(Ordering::Acquire);
            if unlinked_fd >= 0 {
                return Ok(InodeHandle::Fd(unlinked_fd as RawFd));
            }
        }

        Ok(InodeHandle::Path(cstr))
    }

    fn name_to_path(&self, parent: Inode, name: &CStr) -> io::Result<CString> {
        debug!(
            "name_to_path: parent={} name={}",
            parent,
            name.to_string_lossy()
        );
        let data = self
            .inodes
            .read()
            .unwrap()
            .get(&parent)
            .cloned()
            .ok_or_else(ebadf)?;

        let cstr = CString::new(format!(
            "/.vol/{}/{}/{}",
            data.dev,
            data.ino,
            name.to_string_lossy()
        ))
        .map_err(|_| einval())?;
        debug!("name_to_path: path={}", cstr.to_string_lossy());
        Ok(cstr)
    }

    fn open_inode(&self, inode: Inode, mut flags: i32) -> io::Result<File> {
        // When writeback caching is enabled, the kernel may send read requests even if the
        // userspace program opened the file write-only. So we need to ensure that we have opened
        // the file for reading as well as writing.
        let writeback = self.writeback.load(Ordering::Relaxed);
        if writeback && flags & libc::O_ACCMODE == libc::O_WRONLY {
            flags &= !libc::O_ACCMODE;
            flags |= libc::O_RDWR;
        }

        // When writeback caching is enabled the kernel is responsible for handling `O_APPEND`.
        // However, this breaks atomicity as the file may have changed on disk, invalidating the
        // cached copy of the data in the kernel and the offset that the kernel thinks is the end of
        // the file. Just allow this for now as it is the user's responsibility to enable writeback
        // caching only for directories that are not shared. It also means that we need to clear the
        // `O_APPEND` flag.
        if writeback && flags & libc::O_APPEND != 0 {
            flags &= !libc::O_APPEND;
        }

        let ihandle = self.inode_to_handle(inode, true)?;
        let fd = match ihandle {
            InodeHandle::Path(c_path) => unsafe {
                libc::open(
                    c_path.as_ptr(),
                    (flags | libc::O_CLOEXEC) & (!libc::O_NOFOLLOW) & (!libc::O_EXLOCK),
                )
            },
            // Check if we have recently unlinked the inode and kept open a file descriptor to it.
            InodeHandle::Fd(fd) => unsafe { libc::dup(fd) },
        };
        if fd < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }

        // Safe because we just opened this fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn do_readdir<F>(
        &self,
        inode: Inode,
        handle: Handle,
        size: u32,
        mut offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        if size == 0 {
            return Ok(());
        }

        let data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)?;

        let mut ds = data.dirstream.lock().unwrap();

        // We use offset == 0 as an indicator of this being either a fresh directory
        // stream or a stream that has been rewound. If that's the case, make sure
        // the cache will be refreshed.
        if offset == 0 && ds.ready {
            let fd = data.file.write().unwrap().as_raw_fd();
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
            ds.entries.clear();
            ds.ready = false;
        }

        if !ds.ready {
            let fd = data.file.write().unwrap().as_raw_fd();
            let fill_result = if self.unshare_dir().is_some() {
                self.fill_overlay_readdir(inode, fd, &mut ds)
            } else {
                ds.fill_from_fd(fd)
            };

            // Fill the cache on first call.
            if let Err(e) = fill_result {
                if ds.entries.is_empty() {
                    return Err(e);
                }
                // If we got some valid entries before error happened,
                // treat this partial read as success and just log
                // the error.
                warn!("virtio-fs: error in readdir {}: {:?}", inode, e);
            }
            ds.ready = true;
        }

        while let Some(entry) = ds.get_entry(offset) {
            offset += 1;

            let name = entry.name;
            match add_entry(entry) {
                Ok(size) => {
                    if size == 0 {
                        break;
                    }
                }
                Err(e) => {
                    warn!(
                        "virtio-fs: error adding entry {}: {:?}",
                        String::from_utf8_lossy(name),
                        e
                    );
                    break;
                }
            }
        }

        Ok(())
    }

    fn fill_overlay_readdir(
        &self,
        inode: Inode,
        lower_fd: RawFd,
        ds: &mut DirStream,
    ) -> io::Result<()> {
        let rel_path = self.inode_path(inode)?;
        let mut entries = BTreeMap::<Vec<u8>, CachedDirEntry>::new();

        ds.fill_from_fd(lower_fd)?;
        for entry in ds.entries.drain(..) {
            entries.insert(entry.name.to_vec(), entry);
        }
        self.collect_upper_dir_entries(&rel_path, &mut entries)?;
        self.apply_whiteouts(&rel_path, &mut entries)?;

        ds.entries = entries.into_values().collect();
        Ok(())
    }

    fn apply_whiteouts(
        &self,
        rel_path: &Path,
        entries: &mut BTreeMap<Vec<u8>, CachedDirEntry>,
    ) -> io::Result<()> {
        let mut hidden = HashSet::new();
        if let Some(whiteouts) = self.whiteout_path(&rel_path)
            && Self::has_path(&whiteouts)?
        {
            for entry in fs::read_dir(whiteouts).map_err(linux_error)? {
                let entry = entry.map_err(linux_error)?;
                if entry.file_name() == Self::WHITEOUT_MARKER {
                    continue;
                }
                let rel_entry = rel_path.join(entry.file_name());
                if !self.direct_whiteout_exists(&rel_entry)? {
                    continue;
                }
                hidden.insert(
                    entry
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes(),
                );
            }
        }

        entries.retain(|name, _| !hidden.contains(name));
        Ok(())
    }

    fn collect_upper_dir_entries(
        &self,
        rel_path: &Path,
        entries: &mut BTreeMap<Vec<u8>, CachedDirEntry>,
    ) -> io::Result<()> {
        if let Some(upper) = self.upper_path(rel_path)
            && Self::has_path(&upper)?
        {
            self.collect_dir_entries(&upper, entries)?;
        }
        Ok(())
    }

    fn overlay_dir_entries_from_paths(
        &self,
        rel_path: &Path,
    ) -> io::Result<BTreeMap<Vec<u8>, CachedDirEntry>> {
        let lower = self.lower_path(rel_path);
        let mut entries = BTreeMap::<Vec<u8>, CachedDirEntry>::new();

        if Self::has_path(&lower)? {
            self.collect_dir_entries(&lower, &mut entries)?;
        }
        self.collect_upper_dir_entries(rel_path, &mut entries)?;
        self.apply_whiteouts(rel_path, &mut entries)?;
        Ok(entries)
    }

    fn overlay_dir_is_empty(&self, rel_path: &Path) -> io::Result<bool> {
        Ok(self.overlay_dir_entries_from_paths(rel_path)?.is_empty())
    }

    fn validate_lower_unlink(
        &self,
        rel_path: &Path,
        metadata: &fs::Metadata,
        flags: libc::c_int,
    ) -> io::Result<()> {
        if flags == libc::AT_REMOVEDIR {
            if !metadata.file_type().is_dir() {
                return Err(linux_error(io::Error::from_raw_os_error(libc::ENOTDIR)));
            }
            if !self.overlay_dir_is_empty(rel_path)? {
                return Err(linux_error(io::Error::from_raw_os_error(libc::ENOTEMPTY)));
            }
        } else if metadata.file_type().is_dir() {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EISDIR)));
        }
        Ok(())
    }

    fn collect_dir_entries(
        &self,
        path: &Path,
        entries: &mut BTreeMap<Vec<u8>, CachedDirEntry>,
    ) -> io::Result<()> {
        for entry in fs::read_dir(path).map_err(linux_error)? {
            let entry = entry.map_err(linux_error)?;
            let name = entry
                .file_name()
                .to_string_lossy()
                .into_owned()
                .into_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(linux_error)?;
            let type_ = if metadata.file_type().is_dir() {
                libc::DT_DIR
            } else if metadata.file_type().is_symlink() {
                libc::DT_LNK
            } else if metadata.file_type().is_file() {
                libc::DT_REG
            } else {
                libc::DT_UNKNOWN
            } as u8;
            entries.insert(
                name.clone(),
                CachedDirEntry {
                    ino: metadata.ino(),
                    name: name.into_boxed_slice(),
                    type_,
                },
            );
        }
        Ok(())
    }

    fn do_open(
        &self,
        ctx: &Context,
        inode: Inode,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        let flags = self.parse_open_flags(flags as i32);

        let file = RwLock::new(self.open_inode(inode, flags)?);

        // If O_TRUNC and kill_priv (OPEN_KILL_SUIDGID), clear security.capability and suid/sgid
        if (flags & libc::O_TRUNC) != 0 && kill_priv {
            let fd = file.read().unwrap().as_raw_fd();
            let ihandle = InodeHandle::Fd(fd);

            remove_security_capability(&ihandle);

            if let Ok(st) = fstat(ctx, fd, false) {
                let new_mode = clear_suid_sgid(st.st_mode as u32);
                if new_mode != st.st_mode as u32
                    && let Err(err) = set_xattr_stat(ctx, &ihandle, Some(st), None, Some(new_mode))
                {
                    error!("Couldn't clear suid/sgid for inode {inode}: {err}");
                }
            }
        }

        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let data = HandleData {
            inode,
            file,
            dirstream: Mutex::new(DirStream::new()),
        };

        self.handles.write().unwrap().insert(handle, Arc::new(data));

        let mut opts = OpenOptions::empty();
        match self.cfg.cache_policy {
            // We only set the direct I/O option on files.
            CachePolicy::Never => opts.set(OpenOptions::DIRECT_IO, flags & libc::O_DIRECTORY == 0),
            CachePolicy::Always => {
                if flags & libc::O_DIRECTORY == 0 {
                    opts |= OpenOptions::KEEP_CACHE;
                } else {
                    opts |= OpenOptions::CACHE_DIR;
                }
            }
            _ => {}
        };

        Ok((Some(handle), opts))
    }

    fn do_release(&self, inode: Inode, handle: Handle) -> io::Result<()> {
        let mut handles = self.handles.write().unwrap();

        if let btree_map::Entry::Occupied(e) = handles.entry(handle)
            && e.get().inode == inode
        {
            // We don't need to close the file here because that will happen automatically when
            // the last `Arc` is dropped.
            e.remove();
            return Ok(());
        }

        Err(ebadf())
    }

    fn do_getattr(&self, ctx: &Context, inode: Inode) -> io::Result<(bindings::stat64, Duration)> {
        let ihandle = self.inode_to_handle(inode, true)?;
        let st = match ihandle {
            InodeHandle::Path(c_path) => lstat(ctx, &c_path, false)?,
            InodeHandle::Fd(fd) => fstat(ctx, fd, false)?,
        };

        Ok((st, self.cfg.attr_timeout))
    }

    fn grab_unlinked_fd(&self, parent_fd: RawFd, name: &CStr) -> io::Result<RawFd> {
        let fd =
            unsafe { libc::openat(parent_fd, name.as_ptr(), libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    fn store_unlinked_fd(&self, ctx: &Context, unlinked_fd: RawFd) -> io::Result<bool> {
        let st = fstat(ctx, unlinked_fd, true)?;
        let altkey = InodeAltKey {
            ino: st.st_ino,
            dev: st.st_dev,
        };
        // Hold the read lock across the swap: dropping it earlier would let a
        // concurrent `forget` remove this inode (closing its then-`-1`
        // `unlinked_fd`) between our lookup and swap, leaking the fd we store.
        let inodes = self.inodes.read().unwrap();
        if let Some(data) = inodes.get_alt(&altkey) {
            // Swap rather than store so that if this inode already had a
            // preserved fd (e.g. another hard link was unlinked/overwritten
            // earlier), we recover and close it instead of leaking it.
            let old_fd = data.unlinked_fd.swap(unlinked_fd as i64, Ordering::AcqRel);
            if old_fd >= 0 {
                unsafe { libc::close(old_fd as RawFd) };
            }
            // The tracked inode now owns `unlinked_fd` (closed in `forget_one`).
            Ok(true)
        } else {
            // No tracked inode for this (dev, ino): the caller keeps ownership
            // of `unlinked_fd` and must close it to avoid a leak.
            Ok(false)
        }
    }

    fn do_unlink(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        flags: libc::c_int,
    ) -> io::Result<()> {
        let ihandle = self.inode_to_handle(parent, true)?;

        let (fd, close_fd) = match ihandle {
            InodeHandle::Path(c_path) => unsafe {
                (
                    libc::open(c_path.as_ptr(), libc::O_NOFOLLOW | libc::O_CLOEXEC),
                    true,
                )
            },
            InodeHandle::Fd(fd) => (fd, false),
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // After unlinking this inode, we can't keep relying on getting a "/.vol/..." path
        // to operate on it. Before unlinking the inode, grab a file descriptor so we can
        // still operate on it. This one will be closed on "forget_one".
        let unlinked_fd = match self.grab_unlinked_fd(fd, name) {
            Ok(fd) => Some(fd),
            Err(err) => {
                warn!(
                    "Couldn't grab a file descriptor for file \"{}\": {err}",
                    name.to_string_lossy()
                );
                None
            }
        };

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::unlinkat(fd, name.as_ptr(), flags) };
        let err = io::Error::last_os_error();

        if close_fd {
            unsafe { libc::close(fd) };
        }

        if res == 0 {
            if let Some(unlinked_fd) = unlinked_fd {
                match self.store_unlinked_fd(&ctx, unlinked_fd) {
                    // The tracked inode took ownership of the fd.
                    Ok(true) => {}
                    // No tracked inode: we still own the fd and must close it.
                    Ok(false) => unsafe {
                        libc::close(unlinked_fd);
                    },
                    Err(err) => {
                        unsafe { libc::close(unlinked_fd) };
                        warn!("Couldn't store unlinked fd \"{}\": {err}", unlinked_fd);
                    }
                }
            }
            Ok(())
        } else {
            if let Some(unlinked_fd) = unlinked_fd {
                unsafe { libc::close(unlinked_fd) };
            }
            Err(linux_error(err))
        }
    }

    fn overlay_unlink(&self, rel_path: &Path, flags: libc::c_int) -> io::Result<()> {
        if self.whiteout_covers(rel_path)? && !self.direct_whiteout_exists(rel_path)? {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOENT)));
        }

        let upper_removed = if let Some(upper) = self.upper_path(rel_path) {
            match Self::path_metadata(&upper)? {
                Some(metadata) if flags == libc::AT_REMOVEDIR => {
                    if !metadata.file_type().is_dir() {
                        return Err(linux_error(io::Error::from_raw_os_error(libc::ENOTDIR)));
                    }
                    fs::remove_dir(&upper).map_err(linux_error)?;
                    true
                }
                Some(metadata) => {
                    if metadata.file_type().is_dir() {
                        return Err(linux_error(io::Error::from_raw_os_error(libc::EISDIR)));
                    }
                    fs::remove_file(&upper).map_err(linux_error)?;
                    true
                }
                None => false,
            }
        } else {
            false
        };

        let lower = self.lower_path(rel_path);
        if let Some(metadata) = Self::path_metadata(&lower)? {
            if !upper_removed {
                self.validate_lower_unlink(rel_path, &metadata, flags)?;
            }
            self.create_direct_whiteout(rel_path)?;
        } else if !upper_removed {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOENT)));
        } else {
            self.clear_direct_whiteout(rel_path)?;
        }

        self.inode_paths
            .write()
            .unwrap()
            .retain(|_, path| path != rel_path);
        Ok(())
    }

    fn overlay_rename(
        &self,
        ctx: &Context,
        old_rel_path: &Path,
        new_rel_path: &Path,
        flags: u32,
    ) -> io::Result<()> {
        if ((flags as i32) & bindings::LINUX_RENAME_EXCHANGE) != 0
            || ((flags as i32) & bindings::LINUX_RENAME_WHITEOUT) != 0
        {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
        }

        let old_unshared = self.should_unshare_path(old_rel_path)?;
        let new_unshared = self.should_unshare_path(new_rel_path)?;
        if !old_unshared && !new_unshared {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
        }
        if !old_unshared || !new_unshared {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EXDEV)));
        }

        let old_lower_exists = Self::has_path(&self.lower_path(old_rel_path))?;
        self.copy_up_path(ctx, old_rel_path)?;
        self.prepare_upper_parent(ctx, new_rel_path)?;

        if ((flags as i32) & bindings::LINUX_RENAME_NOREPLACE) != 0 {
            if self.path_has_upper(new_rel_path)? || Self::has_path(&self.lower_path(new_rel_path))?
            {
                return Err(linux_error(io::Error::from_raw_os_error(libc::EEXIST)));
            }
        }

        let old_upper = self.upper_path(old_rel_path).ok_or_else(einval)?;
        let new_upper = self.upper_path(new_rel_path).ok_or_else(einval)?;
        self.clear_direct_whiteout(new_rel_path)?;
        fs::rename(&old_upper, &new_upper).map_err(linux_error)?;
        if old_lower_exists {
            self.create_direct_whiteout(old_rel_path)?;
        } else {
            self.clear_direct_whiteout(old_rel_path)?;
        }
        Ok(())
    }

    fn parse_open_flags(&self, flags: i32) -> i32 {
        let mut mflags: i32 = flags & 0b11;

        if (flags & bindings::LINUX_O_NONBLOCK) != 0 {
            mflags |= libc::O_NONBLOCK;
        }
        if (flags & bindings::LINUX_O_APPEND) != 0 {
            mflags |= libc::O_APPEND;
        }
        if (flags & bindings::LINUX_O_CREAT) != 0 {
            mflags |= libc::O_CREAT;
        }
        if (flags & bindings::LINUX_O_TRUNC) != 0 {
            mflags |= libc::O_TRUNC;
        }
        if (flags & bindings::LINUX_O_EXCL) != 0 {
            mflags |= libc::O_EXCL;
        }
        if (flags & bindings::LINUX_O_NOFOLLOW) != 0 {
            mflags |= libc::O_NOFOLLOW;
        }
        if (flags & bindings::LINUX_O_CLOEXEC) != 0 {
            mflags |= libc::O_CLOEXEC;
        }

        mflags
    }
}

fn set_secctx(file: &InodeHandle, secctx: SecContext, symlink: bool) -> io::Result<()> {
    let options = if symlink { libc::XATTR_NOFOLLOW } else { 0 };
    let ret = match file {
        InodeHandle::Path(path) => unsafe {
            libc::setxattr(
                path.as_ptr(),
                secctx.name.as_ptr(),
                secctx.secctx.as_ptr() as *const libc::c_void,
                secctx.secctx.len(),
                0,
                options,
            )
        },
        InodeHandle::Fd(fd) => unsafe {
            libc::fsetxattr(
                *fd,
                secctx.name.as_ptr(),
                secctx.secctx.as_ptr() as *const libc::c_void,
                secctx.secctx.len(),
                0,
                options,
            )
        },
    };

    if ret != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Remove the security.capability extended attribute
fn remove_security_capability(file: &InodeHandle) {
    let ret = match file {
        InodeHandle::Path(path) => unsafe {
            libc::removexattr(path.as_ptr(), SECURITY_CAPABILITY.as_ptr() as *const i8, 0)
        },
        InodeHandle::Fd(fd) => unsafe {
            libc::fremovexattr(*fd, SECURITY_CAPABILITY.as_ptr() as *const i8, 0)
        },
    };

    // ENODATA/ENOATTR mean the attribute didn't exist, which is fine.
    // ENOTSUP means this host filesystem cannot store the attribute anyway.
    if ret != 0 {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENODATA | libc::ENOATTR | libc::ENOTSUP) => {}
            _ => warn!("Error removing security.capability from file: {err}"),
        }
    }
}

/// Clear suid/sgid bits from mode.
/// sgid is cleared only if group executable bit is set.
fn clear_suid_sgid(mode: u32) -> u32 {
    let mut new_mode = mode;

    // Clear suid bit
    new_mode &= !libc::S_ISUID as u32;

    // Clear sgid bit only if group executable bit is set
    if (mode & libc::S_IXGRP as u32) != 0 {
        new_mode &= !libc::S_ISGID as u32;
    }

    new_mode
}

fn forget_one(
    inodes: &mut MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>,
    inode: Inode,
    count: u64,
) {
    if let Some(data) = inodes.get(&inode) {
        // Acquiring the write lock on the inode map prevents new lookups from incrementing the
        // refcount but there is the possibility that a previous lookup already acquired a
        // reference to the inode data and is in the process of updating the refcount so we need
        // to loop here until we can decrement successfully.
        loop {
            let refcount = data.refcount.load(Ordering::Relaxed);

            // Saturating sub because it doesn't make sense for a refcount to go below zero and
            // we don't want misbehaving clients to cause integer overflow.
            let new_count = refcount.saturating_sub(count);

            // Synchronizes with the acquire load in `lookup`.
            if data
                .refcount
                .compare_exchange(refcount, new_count, Ordering::Release, Ordering::Relaxed)
                .unwrap()
                == refcount
            {
                if new_count == 0 {
                    // If we have unlinked this inode, we have opened a file descriptor to be
                    // able to operate on it without a path. Close it now.
                    let fd = data.unlinked_fd.load(Ordering::Acquire);
                    if fd >= 0 {
                        unsafe { libc::close(fd as RawFd) };
                    }
                    // We just removed the last refcount for this inode. There's no need for an
                    // acquire fence here because we hold a write lock on the inode map and any
                    // thread that is waiting to do a forget on the same inode will have to wait
                    // until we release the lock. So there's is no other release store for us to
                    // synchronize with before deleting the entry.
                    inodes.remove(&inode);
                }
                break;
            }
        }
    }
}

impl FileSystem for PassthroughFs {
    type Inode = Inode;
    type Handle = Handle;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        let root = CString::new(self.cfg.root_dir.as_str()).expect("CString::new failed");

        // Safe because this doesn't modify any memory and we check the return value.
        // We use `O_PATH` because we just want this for traversing the directory tree
        // and not for actually reading the contents.
        let fd = unsafe {
            libc::openat(
                libc::AT_FDCWD,
                root.as_ptr(),
                libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because we just opened this fd above.
        let f = unsafe { File::from_raw_fd(fd) };

        let ctx = Context {
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let st = fstat(&ctx, f.as_raw_fd(), true)?;

        // Safe because this doesn't modify any memory and there is no need to check the return
        // value because this system call always succeeds. We need to clear the umask here because
        // we want the client to be able to set all the bits in the mode.
        unsafe { libc::umask(0o000) };

        let mut inodes = self.inodes.write().unwrap();

        // Not sure why the root inode gets a refcount of 2 but that's what libfuse does.
        inodes.insert(
            fuse::ROOT_ID,
            InodeAltKey {
                ino: st.st_ino,
                dev: st.st_dev,
            },
            Arc::new(InodeData {
                inode: fuse::ROOT_ID,
                ino: st.st_ino,
                dev: st.st_dev,
                refcount: AtomicU64::new(2),
                unlinked_fd: AtomicI64::new(-1),
            }),
        );
        self.remember_inode_path(fuse::ROOT_ID, PathBuf::new());

        let mut opts = FsOptions::empty();
        if self.cfg.writeback && capable.contains(FsOptions::WRITEBACK_CACHE) {
            opts |= FsOptions::WRITEBACK_CACHE;
            self.writeback.store(true, Ordering::Relaxed);
        }

        if capable.contains(FsOptions::SUBMOUNTS) {
            opts |= FsOptions::SUBMOUNTS;
            self.announce_submounts.store(true, Ordering::Relaxed);
        }

        Ok(opts)
    }

    fn destroy(&self) {
        self.handles.write().unwrap().clear();
        self.inodes.write().unwrap().clear();
        self.inode_paths.write().unwrap().clear();
    }

    fn statfs(&self, _ctx: Context, inode: Inode) -> io::Result<bindings::statvfs64> {
        let mut out = MaybeUninit::<bindings::statvfs64>::zeroed();

        let res = match self.inode_to_handle(inode, true)? {
            InodeHandle::Path(c_path) => unsafe {
                bindings::statvfs64(c_path.as_ptr(), out.as_mut_ptr())
            },
            InodeHandle::Fd(fd) => unsafe { bindings::fstatvfs64(fd, out.as_mut_ptr()) },
        };
        if res == 0 {
            // Safe because the kernel guarantees that `out` has been initialized.
            Ok(unsafe { out.assume_init() })
        } else {
            Err(linux_error(io::Error::last_os_error()))
        }
    }

    fn lookup(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        let rel_path = self.rel_child_path(parent, name)?;
        let parent_data = self
            .inodes
            .read()
            .unwrap()
            .get(&parent)
            .cloned()
            .ok_or_else(ebadf)?;

        let c_path = match self.overlay_path(parent, name, &rel_path)? {
            OverlayPath::Lower(path) | OverlayPath::Upper(path) => path,
            OverlayPath::Whiteout => {
                return Err(linux_error(io::Error::from_raw_os_error(libc::ENOENT)));
            }
        };
        let st = lstat(&ctx, &c_path, false)?;

        debug!(
            "lookup: inode={} path={}",
            st.st_ino,
            c_path.to_str().unwrap()
        );

        let mut attr_flags: u32 = 0;

        if st.st_mode & libc::S_IFMT == libc::S_IFDIR
            && self.announce_submounts.load(Ordering::Relaxed)
            && (st.st_dev != parent_data.dev)
        {
            attr_flags |= fuse::ATTR_SUBMOUNT;
        }

        let altkey = InodeAltKey {
            ino: st.st_ino,
            dev: st.st_dev,
        };
        let data = self.inodes.read().unwrap().get_alt(&altkey).cloned();

        let inode = if let Some(data) = data {
            // Matches with the release store in `forget`.
            data.refcount.fetch_add(1, Ordering::Acquire);
            data.inode
        } else {
            // There is a possible race here where 2 threads end up adding the same file
            // into the inode list.  However, since each of those will get a unique Inode
            // value and unique file descriptors this shouldn't be that much of a problem.
            let inode = self.inode_alloc.next();
            self.inodes.write().unwrap().insert(
                inode,
                InodeAltKey {
                    ino: st.st_ino,
                    dev: st.st_dev,
                },
                Arc::new(InodeData {
                    inode,
                    ino: st.st_ino,
                    dev: st.st_dev,
                    refcount: AtomicU64::new(1),
                    unlinked_fd: AtomicI64::new(-1),
                }),
            );

            inode
        };
        self.remember_inode_path(inode, rel_path);

        Ok(Entry {
            inode,
            generation: 0,
            attr: st,
            attr_flags,
            attr_timeout: self.cfg.attr_timeout,
            entry_timeout: self.cfg.entry_timeout,
        })
    }

    fn forget(&self, _ctx: Context, inode: Inode, count: u64) {
        let mut inodes = self.inodes.write().unwrap();

        forget_one(&mut inodes, inode, count);
        if inodes.get(&inode).is_none() {
            self.inode_paths.write().unwrap().remove(&inode);
        }
    }

    fn batch_forget(&self, _ctx: Context, requests: Vec<(Inode, u64)>) {
        let mut inodes = self.inodes.write().unwrap();

        for (inode, count) in requests {
            forget_one(&mut inodes, inode, count)
        }
    }

    fn opendir(
        &self,
        ctx: Context,
        inode: Inode,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.do_open(&ctx, inode, false, flags | libc::O_DIRECTORY as u32)
    }

    fn releasedir(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
        handle: Handle,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }

    fn mkdir(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let rel_path = self.check_write_child(parent, name)?;
        let c_path = self.child_write_path(&ctx, parent, name, &rel_path)?;

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::mkdir(c_path.as_ptr(), 0o700) };
        if res == 0 {
            let ihandle = InodeHandle::Path(c_path);
            // Set security context
            if let Some(secctx) = extensions.secctx {
                set_secctx(&ihandle, secctx, false)?
            };

            set_xattr_stat(
                &ctx,
                &ihandle,
                None,
                Some((ctx.uid, ctx.gid)),
                Some(mode & !umask),
            )?;
            let entry = self.lookup(ctx, parent, name)?;
            self.remember_inode_path(entry.inode, rel_path);
            Ok(entry)
        } else {
            Err(linux_error(io::Error::last_os_error()))
        }
    }

    fn rmdir(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        let rel_path = self.check_write_child(parent, name)?;
        if self.should_unshare_path(&rel_path)? {
            return self.overlay_unlink(&rel_path, libc::AT_REMOVEDIR);
        }
        self.do_unlink(ctx, parent, name, libc::AT_REMOVEDIR)
    }

    fn readdir<F>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        self.do_readdir(inode, handle, size, offset, add_entry)
    }

    fn readdirplus<F>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry, Entry) -> io::Result<usize>,
    {
        self.do_readdir(inode, handle, size, offset, |dir_entry| {
            // Safe because the kernel guarantees that the buffer is nul-terminated. Additionally,
            // the kernel will pad the name with '\0' bytes up to 8-byte alignment and there's no
            // way for us to know exactly how many padding bytes there are. This would cause
            // `CStr::from_bytes_with_nul` to return an error because it would think there are
            // interior '\0' bytes. We trust the kernel to provide us with properly formatted data
            // so we'll just skip the checks here.
            let name = unsafe { CStr::from_bytes_with_nul_unchecked(dir_entry.name) };
            let entry = self.lookup(ctx, inode, name)?;

            add_entry(dir_entry, entry)
        })
    }

    fn open(
        &self,
        ctx: Context,
        inode: Inode,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        let f = flags as i32;
        if f & libc::O_ACCMODE != libc::O_RDONLY || f & libc::O_TRUNC != 0 {
            self.check_write_inode(inode)?;
            self.ensure_unshared_inode(&ctx, inode)?;
        }
        self.do_open(&ctx, inode, kill_priv, flags)
    }

    fn release(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
        handle: Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }

    fn create(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        kill_priv: bool,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions)> {
        let rel_path = self.check_write_child(parent, name)?;
        let c_path = self.child_write_path(&ctx, parent, name, &rel_path)?;

        let flags = self.parse_open_flags(flags as i32);
        let hostmode = if (flags & libc::O_DIRECTORY) != 0 {
            0o700
        } else {
            0o600
        };

        // Safe because this doesn't modify any memory and we check the return value. We don't
        // really check `flags` because if the kernel can't handle poorly specified flags then we
        // have much bigger problems.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                flags | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                hostmode,
            )
        };
        if fd < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }
        let ihandle = InodeHandle::Fd(fd);

        if let Err(e) = set_xattr_stat(
            &ctx,
            &ihandle,
            None,
            Some((ctx.uid, ctx.gid)),
            Some(libc::S_IFREG as u32 | (mode & !(umask & 0o777))),
        ) {
            unsafe { libc::close(fd) };
            return Err(e);
        }

        // Set security context
        if let Some(secctx) = extensions.secctx {
            set_secctx(&ihandle, secctx, false)?
        };

        // If O_TRUNC and kill_priv (OPEN_KILL_SUIDGID), clear security.capability.
        // We don't need to clear suid/sgid here because we've just updated them
        // unconditionally above.
        if (flags & libc::O_TRUNC) != 0 && kill_priv {
            remove_security_capability(&ihandle);
        }

        // Safe because we just opened this fd.
        let file = RwLock::new(unsafe { File::from_raw_fd(fd) });

        let entry = self.lookup(ctx, parent, name)?;
        self.remember_inode_path(entry.inode, rel_path);

        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let data = HandleData {
            inode: entry.inode,
            file,
            dirstream: Mutex::new(DirStream::new()),
        };

        self.handles.write().unwrap().insert(handle, Arc::new(data));

        let mut opts = OpenOptions::empty();
        match self.cfg.cache_policy {
            CachePolicy::Never => opts |= OpenOptions::DIRECT_IO,
            CachePolicy::Always => opts |= OpenOptions::KEEP_CACHE,
            _ => {}
        };

        Ok((entry, Some(handle), opts))
    }

    fn unlink(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        let rel_path = self.check_write_child(parent, name)?;
        if self.should_unshare_path(&rel_path)? {
            return self.overlay_unlink(&rel_path, 0);
        }
        self.do_unlink(ctx, parent, name, 0)
    }

    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        mut w: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        debug!("read: {inode:?}");
        let data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)?;

        // This is safe because write_from uses preadv64, so the underlying file descriptor
        // offset is not affected by this operation.
        let f = data.file.read().unwrap();
        w.write_from(&f, size as usize, offset)
    }

    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        mut r: R,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        kill_priv: bool,
        _flags: u32,
    ) -> io::Result<usize> {
        self.check_write_inode(inode)?;
        let data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)?;

        // This is safe because read_to uses pwritev64, so the underlying file descriptor
        // offset is not affected by this operation.
        let f = data.file.read().unwrap();
        let result = r.read_to(&f, size as usize, offset);

        // If write succeeded and kill_priv is set, clear security.capability and suid/sgid
        if result.is_ok() && kill_priv {
            let fd = f.as_raw_fd();
            let ihandle = InodeHandle::Fd(fd);

            remove_security_capability(&ihandle);

            if let Ok(st) = fstat(&ctx, fd, false) {
                let new_mode = clear_suid_sgid(st.st_mode as u32);
                if new_mode != st.st_mode as u32 {
                    // Update mode in xattr
                    if let Err(err) = set_xattr_stat(&ctx, &ihandle, Some(st), None, Some(new_mode))
                    {
                        error!("Couldn't clear suid/sgid for inode {inode}: {err}");
                    }
                }
            }
        }

        result
    }

    fn getattr(
        &self,
        ctx: Context,
        inode: Inode,
        _handle: Option<Handle>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.do_getattr(&ctx, inode)
    }

    fn setattr(
        &self,
        ctx: Context,
        inode: Inode,
        attr: bindings::stat64,
        handle: Option<Handle>,
        valid: SetattrValid,
    ) -> io::Result<(bindings::stat64, Duration)> {
        if !valid.is_empty() {
            self.check_write_inode(inode)?;
            self.ensure_unshared_inode(&ctx, inode)?;
        }
        // If we have a handle then use it otherwise get a new fd from the inode.
        let use_inode_handle = self
            .inode_path(inode)
            .map(|path| self.path_has_upper(&path).unwrap_or(false))
            .unwrap_or(false);
        let ihandle = if let Some(handle) = handle
            && !use_inode_handle
        {
            let hd = self
                .handles
                .read()
                .unwrap()
                .get(&handle)
                .filter(|hd| hd.inode == inode)
                .cloned()
                .ok_or_else(ebadf)?;

            let fd = hd.file.write().unwrap().as_raw_fd();
            InodeHandle::Fd(fd)
        } else {
            self.inode_to_handle(inode, true)?
        };

        if valid.contains(SetattrValid::MODE) {
            chmod_host(&ihandle, attr.st_mode as u32)?;
            clear_xattr_mode(&ctx, &ihandle, None)?;
        }

        if valid.intersects(SetattrValid::UID | SetattrValid::GID) {
            let uid = if valid.contains(SetattrValid::UID) {
                attr.st_uid
            } else {
                // Cannot use -1 here because these are unsigned values.
                u32::MAX
            };
            let gid = if valid.contains(SetattrValid::GID) {
                attr.st_gid
            } else {
                // Cannot use -1 here because these are unsigned values.
                u32::MAX
            };

            remove_security_capability(&ihandle);
            let st = istat(&ctx, &ihandle, false)?;

            // Clear suid/sgid if UID or GID is being changed
            let new_mode = clear_suid_sgid(st.st_mode as u32);
            let new_mode = if new_mode != st.st_mode as u32 {
                Some(new_mode)
            } else {
                None
            };
            set_xattr_stat(&ctx, &ihandle, Some(st), Some((uid, gid)), new_mode)?;
        }

        if valid.contains(SetattrValid::SIZE) {
            // Safe because this doesn't modify any memory and we check the return value.
            match ihandle {
                InodeHandle::Fd(fd) => {
                    let res = unsafe { libc::ftruncate(fd, attr.st_size) };
                    if res < 0 {
                        return Err(linux_error(io::Error::last_os_error()));
                    }

                    // Clear security.capability on truncate unconditionally
                    remove_security_capability(&ihandle);
                    let st = fstat(&ctx, fd, false)?;
                    let new_mode = clear_suid_sgid(st.st_mode as u32);
                    if new_mode != st.st_mode as u32 {
                        set_xattr_stat(&ctx, &ihandle, Some(st), None, Some(new_mode))?;
                    }
                }
                InodeHandle::Path(_) => {
                    // There is no `ftruncateat` so we need to get a new fd and truncate it.
                    let f = self.open_inode(inode, libc::O_NONBLOCK | libc::O_RDWR)?;
                    let res = unsafe { libc::ftruncate(f.as_raw_fd(), attr.st_size) };
                    if res < 0 {
                        return Err(linux_error(io::Error::last_os_error()));
                    }

                    // Clear security.capability on truncate unconditionally
                    //
                    // Do this here even if it means duplicating the code above to be able to
                    // reuse the FD we just opened, thus reducing the number of syscalls.
                    let ihandle = InodeHandle::Fd(f.as_raw_fd());
                    remove_security_capability(&ihandle);
                    let st = istat(&ctx, &ihandle, false)?;
                    let new_mode = clear_suid_sgid(st.st_mode as u32);
                    if new_mode != st.st_mode as u32 {
                        set_xattr_stat(&ctx, &ihandle, Some(st), None, Some(new_mode))?;
                    }
                }
            };
        }

        if valid.intersects(SetattrValid::ATIME | SetattrValid::MTIME) {
            let mut tvs = [
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
            ];

            if valid.contains(SetattrValid::ATIME_NOW) {
                tvs[0].tv_nsec = libc::UTIME_NOW;
            } else if valid.contains(SetattrValid::ATIME) {
                tvs[0].tv_sec = attr.st_atime;
                tvs[0].tv_nsec = attr.st_atime_nsec;
            }

            if valid.contains(SetattrValid::MTIME_NOW) {
                tvs[1].tv_nsec = libc::UTIME_NOW;
            } else if valid.contains(SetattrValid::MTIME) {
                tvs[1].tv_sec = attr.st_mtime;
                tvs[1].tv_nsec = attr.st_mtime_nsec;
            }

            // Safe because this doesn't modify any memory and we check the return value.
            let res = match ihandle {
                InodeHandle::Fd(fd) => unsafe { libc::futimens(fd, tvs.as_ptr()) },
                InodeHandle::Path(c_path) => unsafe {
                    let fd = libc::open(c_path.as_ptr(), libc::O_SYMLINK | libc::O_CLOEXEC);
                    let res = libc::futimens(fd, tvs.as_ptr());
                    libc::close(fd);
                    res
                },
            };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        self.do_getattr(&ctx, inode)
    }

    fn rename(
        &self,
        ctx: Context,
        olddir: Inode,
        oldname: &CStr,
        newdir: Inode,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        let old_rel_path = self.check_write_child(olddir, oldname)?;
        let new_rel_path = self.check_write_child(newdir, newname)?;
        if self.should_unshare_path(&old_rel_path)? || self.should_unshare_path(&new_rel_path)? {
            self.overlay_rename(&ctx, &old_rel_path, &new_rel_path, flags)?;
            let moved = self.move_inode_path(&old_rel_path, &new_rel_path);
            for inode in moved {
                self.refresh_inode_backing(&ctx, inode, &new_rel_path)?;
            }
            return Ok(());
        }
        let mut mflags: u32 = 0;
        if ((flags as i32) & bindings::LINUX_RENAME_NOREPLACE) != 0 {
            mflags |= libc::RENAME_EXCL;
        }
        if ((flags as i32) & bindings::LINUX_RENAME_EXCHANGE) != 0 {
            mflags |= libc::RENAME_SWAP;
        }

        if ((flags as i32) & bindings::LINUX_RENAME_WHITEOUT) != 0
            && ((flags as i32) & bindings::LINUX_RENAME_EXCHANGE) != 0
        {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
        }

        let old_cpath = self.name_to_path(olddir, oldname)?;
        let new_cpath = self.name_to_path(newdir, newname)?;

        // macOS addresses inodes by their volfs path ("/.vol/{dev}/{ino}"),
        // which only resolves while the inode still has a directory entry. A
        // rename that REPLACES an existing target drops that target's last
        // link, so any inode the guest still holds open there would afterwards
        // resolve to a dangling volfs path and fail path-based ops
        // (getattr/open/setattr/...) with ENOENT (e.g. apt/dpkg's atomic
        // rewrite of /var/lib/dpkg/status, surfaced as
        // "close (2: No such file or directory)"). `do_unlink` already guards
        // the unlink case by stashing an fd to the doomed inode in
        // `InodeData.unlinked_fd`; mirror that for the overwritten target. Grab
        // it *before* the rename, while its entry still exists. RENAME_SWAP
        // keeps both inodes linked and RENAME_EXCL never overwrites, so skip
        // those; best-effort otherwise (a non-overwriting rename finds nothing).
        let doomed_fd = if (flags as i32)
            & (bindings::LINUX_RENAME_EXCHANGE | bindings::LINUX_RENAME_NOREPLACE)
            == 0
        {
            match self.inode_to_handle(newdir, true) {
                Ok(InodeHandle::Path(newdir_cpath)) => {
                    let newdir_fd = unsafe {
                        libc::open(newdir_cpath.as_ptr(), libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    };
                    if newdir_fd < 0 {
                        None
                    } else {
                        let grabbed = self.grab_unlinked_fd(newdir_fd, newname).ok();
                        unsafe { libc::close(newdir_fd) };
                        grabbed
                    }
                }
                Ok(InodeHandle::Fd(newdir_fd)) => self.grab_unlinked_fd(newdir_fd, newname).ok(),
                Err(_) => None,
            }
        } else {
            None
        };

        let res = unsafe { libc::renamex_np(old_cpath.as_ptr(), new_cpath.as_ptr(), mflags) };
        if res == 0 {
            // If the rename overwrote a tracked inode, hand its preserved fd to
            // the inode store so later ops resolve by fd, not the vanished path.
            // `store_unlinked_fd` takes ownership only when that inode is
            // tracked; close the fd ourselves otherwise so it is never leaked.
            if let Some(fd) = doomed_fd {
                match self.store_unlinked_fd(&ctx, fd) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => unsafe {
                        libc::close(fd);
                    },
                }
            }

            if ((flags as i32) & bindings::LINUX_RENAME_WHITEOUT) != 0 {
                let fd = unsafe {
                    libc::open(
                        old_cpath.as_ptr(),
                        libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                        0o600,
                    )
                };
                if fd > 0 {
                    if let Err(e) = set_xattr_stat(
                        &ctx,
                        &InodeHandle::Fd(fd),
                        None,
                        None,
                        Some((libc::S_IFCHR | 0o600) as u32),
                    ) {
                        unsafe { libc::close(fd) };
                        return Err(e);
                    }
                    unsafe { libc::close(fd) };
                }
            }

            let entry = self.lookup(ctx, newdir, newname)?;
            self.remember_inode_path(entry.inode, new_rel_path);
            self.forget(ctx, entry.inode, 1);
            self.inode_paths
                .write()
                .unwrap()
                .retain(|_, path| path != &old_rel_path);

            Ok(())
        } else {
            if let Some(fd) = doomed_fd {
                // The rename failed; nothing was overwritten. Drop the fd.
                unsafe { libc::close(fd) };
            }
            Err(linux_error(io::Error::last_os_error()))
        }
    }

    fn mknod(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        _rdev: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let rel_path = self.check_write_child(parent, name)?;
        let c_path = self.child_write_path(&ctx, parent, name, &rel_path)?;

        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            Err(linux_error(io::Error::last_os_error()))
        } else {
            let ihandle = InodeHandle::Fd(fd);

            // Set security context
            if let Some(secctx) = extensions.secctx {
                set_secctx(&ihandle, secctx, false)?
            };

            if let Err(e) = set_xattr_stat(
                &ctx,
                &ihandle,
                None,
                Some((ctx.uid, ctx.gid)),
                Some(mode & !umask),
            ) {
                unsafe { libc::close(fd) };
                return Err(e);
            }

            unsafe { libc::close(fd) };
            let entry = self.lookup(ctx, parent, name)?;
            self.remember_inode_path(entry.inode, rel_path);
            Ok(entry)
        }
    }

    fn link(
        &self,
        ctx: Context,
        inode: Inode,
        newparent: Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        let rel_path = self.check_write_child(newparent, newname)?;
        if self.should_unshare_path(&rel_path)? {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EXDEV)));
        }
        let orig_c_path = match self.inode_to_handle(inode, false)? {
            InodeHandle::Path(c_path) => c_path,
            InodeHandle::Fd(_) => return Err(ebadf()),
        };
        let link_c_path = self.name_to_path(newparent, newname)?;

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::link(orig_c_path.as_ptr(), link_c_path.as_ptr()) };
        if res == 0 {
            let entry = self.lookup(ctx, newparent, newname)?;
            self.remember_inode_path(entry.inode, rel_path);
            Ok(entry)
        } else {
            Err(linux_error(io::Error::last_os_error()))
        }
    }

    fn symlink(
        &self,
        ctx: Context,
        linkname: &CStr,
        parent: Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let rel_path = self.check_write_child(parent, name)?;
        let c_path = self.child_write_path(&ctx, parent, name, &rel_path)?;

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::symlink(linkname.as_ptr(), c_path.as_ptr()) };
        if res == 0 {
            let ihandle = InodeHandle::Path(c_path);

            // Set security context
            if let Some(secctx) = extensions.secctx {
                set_secctx(&ihandle, secctx, true)?
            };

            let mut entry = self.lookup(ctx, parent, name)?;
            let mode = libc::S_IFLNK | 0o777;
            set_xattr_stat(
                &ctx,
                &ihandle,
                None,
                Some((ctx.uid, ctx.gid)),
                Some(mode as u32),
            )?;
            entry.attr.st_uid = ctx.uid;
            entry.attr.st_gid = ctx.gid;
            entry.attr.st_mode = mode;
            self.remember_inode_path(entry.inode, rel_path);
            Ok(entry)
        } else {
            Err(linux_error(io::Error::last_os_error()))
        }
    }

    fn readlink(&self, _ctx: Context, inode: Inode) -> io::Result<Vec<u8>> {
        let mut buf = vec![0; libc::PATH_MAX as usize];

        let res = match self.inode_to_handle(inode, true)? {
            InodeHandle::Path(c_path) => unsafe {
                libc::readlink(
                    c_path.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_char,
                    buf.len(),
                )
            },
            InodeHandle::Fd(fd) => unsafe {
                libc::freadlink(fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) as isize
            },
        };
        if res < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }

        buf.resize(res as usize, 0);
        Ok(buf)
    }

    fn flush(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        let data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)?;

        // Since this method is called whenever an fd is closed in the client, we can emulate that
        // behavior by doing the same thing (dup-ing the fd and then immediately closing it). Safe
        // because this doesn't modify any memory and we check the return values.
        unsafe {
            let newfd = libc::dup(data.file.write().unwrap().as_raw_fd());
            if newfd < 0 {
                return Err(linux_error(io::Error::last_os_error()));
            }

            if libc::close(newfd) < 0 {
                Err(linux_error(io::Error::last_os_error()))
            } else {
                Ok(())
            }
        }
    }

    fn fsync(
        &self,
        _ctx: Context,
        inode: Inode,
        _datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        let data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)?;

        let fd = data.file.write().unwrap().as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::fsync(fd) };

        if res == 0 {
            Ok(())
        } else {
            Err(linux_error(io::Error::last_os_error()))
        }
    }

    fn fsyncdir(
        &self,
        ctx: Context,
        inode: Inode,
        datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        self.fsync(ctx, inode, datasync, handle)
    }

    fn access(&self, ctx: Context, inode: Inode, mask: u32) -> io::Result<()> {
        if (mask as i32 & libc::W_OK) != 0 {
            self.check_write_inode(inode)?;
        }
        let st = match self.inode_to_handle(inode, true)? {
            InodeHandle::Path(c_path) => lstat(&ctx, &c_path, false)?,
            InodeHandle::Fd(fd) => fstat(&ctx, fd, false)?,
        };

        let mode = mask as i32 & (libc::R_OK | libc::W_OK | libc::X_OK);

        if mode == libc::F_OK {
            // The file exists since we were able to call `stat(2)` on it.
            return Ok(());
        }

        // We use ctx.uid/ctx.gid for these checks, but when idmapped mounts
        // support is enabled on the guest side, it means that "default_permissions"
        // flag is set on virtiofs mount and FUSE_ACCESS request should never be
        // sent to the userspace. Please, refer to the kernel commit
        // ("fs/fuse: warn if fuse_access is called when idmapped mounts are allowed").
        // In case when idmapped mounts are not enabled we are good to rely on ctx.uid/ctx.gid values.

        if (mode & libc::R_OK) != 0
            && ctx.uid != 0
            && (st.st_uid != ctx.uid || st.st_mode & 0o400 == 0)
            && (st.st_gid != ctx.gid || st.st_mode & 0o040 == 0)
            && st.st_mode & 0o004 == 0
        {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EACCES)));
        }

        if (mode & libc::W_OK) != 0
            && ctx.uid != 0
            && (st.st_uid != ctx.uid || st.st_mode & 0o200 == 0)
            && (st.st_gid != ctx.gid || st.st_mode & 0o020 == 0)
            && st.st_mode & 0o002 == 0
        {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EACCES)));
        }

        // root can only execute something if it is executable by one of the owner, the group, or
        // everyone.
        if (mode & libc::X_OK) != 0
            && (ctx.uid != 0 || st.st_mode & 0o111 == 0)
            && (st.st_uid != ctx.uid || st.st_mode & 0o100 == 0)
            && (st.st_gid != ctx.gid || st.st_mode & 0o010 == 0)
            && st.st_mode & 0o001 == 0
        {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EACCES)));
        }

        Ok(())
    }

    fn setxattr(
        &self,
        ctx: Context,
        inode: Inode,
        name: &CStr,
        value: &[u8],
        flags: u32,
    ) -> io::Result<()> {
        self.check_write_inode(inode)?;
        self.ensure_unshared_inode(&ctx, inode)?;
        debug!("setxattr: inode={inode} name={name:?} value={value:?}");

        if !self.cfg.xattr {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOSYS)));
        }

        if name.to_bytes() == XATTR_KEY {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EACCES)));
        }

        let mut mflags: i32 = 0;
        if (flags as i32) & bindings::LINUX_XATTR_CREATE != 0 {
            mflags |= libc::XATTR_CREATE;
        }
        if (flags as i32) & bindings::LINUX_XATTR_REPLACE != 0 {
            mflags |= libc::XATTR_REPLACE;
        }

        // Safe because this doesn't modify any memory and we check the return value.
        let res = match self.inode_to_handle(inode, true)? {
            InodeHandle::Path(c_path) => unsafe {
                libc::setxattr(
                    c_path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr() as *const libc::c_void,
                    value.len(),
                    0,
                    mflags as libc::c_int,
                )
            },
            InodeHandle::Fd(fd) => unsafe {
                libc::fsetxattr(
                    fd,
                    name.as_ptr(),
                    value.as_ptr() as *const libc::c_void,
                    value.len(),
                    0,
                    mflags as libc::c_int,
                )
            },
        };

        if res == 0 {
            Ok(())
        } else {
            Err(linux_error(io::Error::last_os_error()))
        }
    }

    fn getxattr(
        &self,
        _ctx: Context,
        inode: Inode,
        name: &CStr,
        size: u32,
    ) -> io::Result<GetxattrReply> {
        debug!("getxattr: inode={inode} name={name:?}, size={size}");

        if !self.cfg.xattr {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOSYS)));
        }

        if name.to_bytes() == XATTR_KEY {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EACCES)));
        }

        let mut buf = vec![0; size as usize];

        // Safe because this will only modify the contents of `buf`
        let res = match self.inode_to_handle(inode, true)? {
            InodeHandle::Path(c_path) => unsafe {
                if size == 0 {
                    libc::getxattr(
                        c_path.as_ptr(),
                        name.as_ptr(),
                        std::ptr::null_mut(),
                        size as libc::size_t,
                        0,
                        0,
                    )
                } else {
                    libc::getxattr(
                        c_path.as_ptr(),
                        name.as_ptr(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        size as libc::size_t,
                        0,
                        0,
                    )
                }
            },
            InodeHandle::Fd(fd) => unsafe {
                if size == 0 {
                    libc::fgetxattr(
                        fd,
                        name.as_ptr(),
                        std::ptr::null_mut(),
                        size as libc::size_t,
                        0,
                        0,
                    )
                } else {
                    libc::fgetxattr(
                        fd,
                        name.as_ptr(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        size as libc::size_t,
                        0,
                        0,
                    )
                }
            },
        };
        if res < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }

        if size == 0 {
            Ok(GetxattrReply::Count(res as u32))
        } else {
            buf.resize(res as usize, 0);
            Ok(GetxattrReply::Value(buf))
        }
    }

    fn listxattr(&self, _ctx: Context, inode: Inode, size: u32) -> io::Result<ListxattrReply> {
        if !self.cfg.xattr {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOSYS)));
        }

        let mut buf = vec![0; 512_usize];

        // Safe because this will only modify the contents of `buf`.
        let res = match self.inode_to_handle(inode, true)? {
            InodeHandle::Path(c_path) => unsafe {
                libc::listxattr(
                    c_path.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_char,
                    512,
                    0,
                )
            },
            InodeHandle::Fd(fd) => unsafe {
                libc::flistxattr(fd, buf.as_mut_ptr() as *mut libc::c_char, 512, 0)
            },
        };
        if res < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }

        buf.truncate(res as usize);

        if size == 0 {
            let mut clean_size = res as usize;

            for attr in buf.split(|c| *c == 0) {
                if attr.starts_with(&XATTR_KEY[..XATTR_KEY.len() - 1]) {
                    clean_size -= XATTR_KEY.len();
                }
            }

            Ok(ListxattrReply::Count(clean_size as u32))
        } else {
            let mut clean_buf = Vec::new();

            for attr in buf.split(|c| *c == 0) {
                if attr.is_empty() || attr.starts_with(&XATTR_KEY[..XATTR_KEY.len() - 1]) {
                    continue;
                }

                clean_buf.extend_from_slice(attr);
                clean_buf.push(0);
            }

            clean_buf.shrink_to_fit();

            if clean_buf.len() > size as usize {
                Err(io::Error::from_raw_os_error(LINUX_ERANGE))
            } else {
                Ok(ListxattrReply::Names(clean_buf))
            }
        }
    }

    fn removexattr(&self, ctx: Context, inode: Inode, name: &CStr) -> io::Result<()> {
        self.check_write_inode(inode)?;
        self.ensure_unshared_inode(&ctx, inode)?;
        if !self.cfg.xattr {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOSYS)));
        }

        if name.to_bytes() == XATTR_KEY {
            return Err(linux_error(io::Error::from_raw_os_error(
                bindings::LINUX_EACCES,
            )));
        }

        // Safe because this doesn't modify any memory and we check the return value.
        let res = match self.inode_to_handle(inode, true)? {
            InodeHandle::Path(c_path) => unsafe {
                libc::removexattr(c_path.as_ptr(), name.as_ptr(), 0)
            },
            InodeHandle::Fd(fd) => unsafe { libc::fremovexattr(fd, name.as_ptr(), 0) },
        };
        if res == 0 {
            Ok(())
        } else {
            Err(linux_error(io::Error::last_os_error()))
        }
    }

    fn fallocate(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        self.check_write_inode(inode)?;
        self.ensure_unshared_inode(&ctx, inode)?;
        let data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)?;

        let fd = data.file.write().unwrap().as_raw_fd();

        const SUPPORTED_FLAGS: i32 = bindings::LINUX_FALLOC_FL_ALLOCATE_RANGE
            | bindings::LINUX_FALLOC_FL_KEEP_SIZE
            | bindings::LINUX_FALLOC_FL_PUNCH_HOLE;

        if mode as i32 & !SUPPORTED_FLAGS != 0 {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EOPNOTSUPP)));
        }

        let keep_size = mode & bindings::LINUX_FALLOC_FL_KEEP_SIZE as u32 != 0;
        let mode = mode & !bindings::LINUX_FALLOC_FL_KEEP_SIZE as u32;

        match mode as i32 {
            bindings::LINUX_FALLOC_FL_ALLOCATE_RANGE => {
                // The closest thing we have on macOS to posix_fallocate is F_PREALLOCATE,
                // but this one doesn't allow us to allocate arbitrary ranges, only allocate
                // blocks to the file's end.
                //
                // The best thing we can do here is extend the file to (offset + length).
                // This doesn't adhere to the same semantics, but should work fine (albeit
                // less performant) for most guest applications.
                let st = fstat(&ctx, fd, true)?;
                let new_length = (offset + length) as i64;

                if keep_size {
                    // Check the number of allocated blocks instead of the file size.
                    let disk_size = st.st_blocks * 512_i64;
                    if disk_size >= new_length {
                        return Ok(());
                    }
                    let mut fs = libc::fstore_t {
                        fst_flags: libc::F_ALLOCATEALL,
                        fst_posmode: libc::F_PEOFPOSMODE,
                        fst_offset: 0,
                        fst_length: new_length - disk_size,
                        fst_bytesalloc: 0,
                    };

                    let res = unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut fs as *mut _) };
                    if res < 0 {
                        return Err(linux_error(io::Error::last_os_error()));
                    }
                } else {
                    if st.st_size >= new_length {
                        return Ok(());
                    }
                    let res = unsafe { libc::ftruncate(fd, new_length) };
                    if res < 0 {
                        return Err(linux_error(io::Error::last_os_error()));
                    }
                }
            }
            bindings::LINUX_FALLOC_FL_PUNCH_HOLE => {
                if !keep_size {
                    // Linux forbids the use of PUNCH_HOLE without KEEP_SIZE.
                    return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
                }

                punch_hole(fd, offset, length)?;
            }
            _ => unreachable!(),
        }

        Ok(())
    }

    fn lseek(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        let data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)?;

        // SEEK_DATA and SEEK_HOLE have slightly different semantics
        // in Linux vs. macOS, which means we can't support them.
        let mwhence = if whence == 3 {
            // SEEK_DATA
            return Ok(offset);
        } else if whence == 4 {
            // SEEK_HOLE
            libc::SEEK_END
        } else {
            whence as i32
        };

        let fd = data.file.write().unwrap().as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::lseek(fd, offset as bindings::off64_t, mwhence as libc::c_int) };
        if res < 0 {
            Err(linux_error(io::Error::last_os_error()))
        } else {
            Ok(res as u64)
        }
    }

    fn copyfilerange(
        &self,
        ctx: Context,
        inode_in: Inode,
        handle_in: Handle,
        offset_in: u64,
        inode_out: Inode,
        handle_out: Handle,
        offset_out: u64,
        len: u64,
        flags: u64,
    ) -> io::Result<usize> {
        if flags != 0 {
            return Err(einval());
        }
        self.check_write_inode(inode_out)?;
        self.ensure_unshared_inode(&ctx, inode_out)?;

        let data_in = self
            .handles
            .read()
            .unwrap()
            .get(&handle_in)
            .filter(|hd| hd.inode == inode_in)
            .cloned()
            .ok_or_else(ebadf)?;
        let fd_in = data_in.file.read().unwrap().as_raw_fd();

        let data_out = self
            .handles
            .read()
            .unwrap()
            .get(&handle_out)
            .filter(|hd| hd.inode == inode_out)
            .cloned()
            .ok_or_else(ebadf)?;
        let fd_out = data_out.file.read().unwrap().as_raw_fd();

        self.copy_sparse_range(&ctx, fd_in, offset_in, fd_out, offset_out, len)
    }

    fn setupmapping(
        &self,
        ctx: Context,
        inode: Inode,
        _handle: Handle,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        guest_shm_base: u64,
        shm_size: u64,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        if (flags & fuse::SetupmappingFlags::WRITE.bits()) != 0 {
            self.check_write_inode(inode)?;
            self.ensure_unshared_inode(&ctx, inode)?;
        }
        if map_sender.is_none() {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOSYS)));
        }

        if (moffset + len) > shm_size {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
        }

        let guest_addr = guest_shm_base + moffset;

        debug!("setupmapping: ino {inode:?} guest_addr={guest_addr:x} len={len}");
        self.install_dax_mapping(
            DaxMappingSnapshot {
                guest_addr,
                inode,
                foffset,
                len,
                writable: (flags & fuse::SetupmappingFlags::WRITE.bits()) != 0,
            },
            map_sender,
        )
    }

    fn removemapping(
        &self,
        _ctx: Context,
        requests: Vec<fuse::RemovemappingOne>,
        guest_shm_base: u64,
        shm_size: u64,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        if map_sender.is_none() {
            return Err(linux_error(io::Error::from_raw_os_error(libc::ENOSYS)));
        }

        for req in requests {
            let guest_addr = guest_shm_base + req.moffset;
            if (req.moffset + req.len) > shm_size {
                return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
            }
            let mapping = match self.map_windows.lock().unwrap().remove(&guest_addr) {
                Some(a) => a,
                None => return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL))),
            };
            debug!(
                "removemapping: guest_addr={:x} len={:?}",
                guest_addr, req.len
            );

            let sender = map_sender.as_ref().unwrap();
            let (reply_sender, reply_receiver) = unbounded();
            sender
                .send(WorkerMessage::GpuRemoveMapping(
                    reply_sender,
                    guest_addr,
                    req.len,
                ))
                .unwrap();
            if !reply_receiver.recv().unwrap() {
                error!("Error requesting HVF the removal of a DAX window");
                return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
            }

            let ret = unsafe {
                libc::munmap(
                    mapping.host_addr as *mut libc::c_void,
                    mapping.snapshot.len as usize,
                )
            };
            if ret == -1 {
                error!("Error unmapping DAX window");
                return Err(linux_error(io::Error::last_os_error()));
            }
        }

        Ok(())
    }
}

unsafe fn mmap_dax_aligned(
    len: usize,
    alignment: usize,
    prot: libc::c_int,
    flags: libc::c_int,
    fd: libc::c_int,
    offset: libc::off_t,
) -> *mut libc::c_void {
    // HVF accepts the virtio-fs DAX windows only when the host VA has the same
    // hugepage-scale alignment as the guest window requested by Linux.
    let reserve_len = match len.checked_add(alignment) {
        Some(size) => size,
        None => return libc::MAP_FAILED,
    };
    let reservation = unsafe {
        libc::mmap(
            null_mut(),
            reserve_len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if reservation == libc::MAP_FAILED {
        return libc::MAP_FAILED;
    }

    let base = reservation as usize;
    let aligned = (base + alignment - 1) & !(alignment - 1);
    let prefix_len = aligned - base;
    if prefix_len != 0 {
        unsafe {
            libc::munmap(reservation, prefix_len);
        }
    }

    let suffix_start = aligned + len;
    let reservation_end = base + reserve_len;
    if suffix_start < reservation_end {
        unsafe {
            libc::munmap(
                suffix_start as *mut libc::c_void,
                reservation_end - suffix_start,
            );
        }
    }

    let mapped = unsafe {
        libc::mmap(
            aligned as *mut libc::c_void,
            len,
            prot,
            flags | libc::MAP_FIXED,
            fd,
            offset,
        )
    };
    if mapped == libc::MAP_FAILED && len != 0 {
        unsafe {
            libc::munmap(aligned as *mut libc::c_void, len);
        }
    }
    mapped
}
