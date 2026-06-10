#[cfg(target_os = "macos")]
use crossbeam_channel::Sender;
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;

use super::filesystem::{
    Context, DirEntry, Entry, Extensions, FileSystem, FsOptions, GetxattrReply, ListxattrReply,
    OpenOptions, SetattrValid, ZeroCopyReader, ZeroCopyWriter,
};
use super::fuse;
#[cfg(target_os = "macos")]
use super::passthrough::{PassthroughFs, PassthroughFsSnapshot};
use crate::virtio::bindings;

type Inode = u64;
type Handle = u64;

pub(crate) struct WritableAllowlistFs<T> {
    inner: T,
    allowlist: Arc<RwLock<Vec<PathBuf>>>,
    inode_paths: RwLock<BTreeMap<Inode, PathBuf>>,
}

impl<T: FileSystem<Inode = Inode, Handle = Handle>> WritableAllowlistFs<T> {
    pub(crate) fn new(inner: T, allowlist: Arc<RwLock<Vec<PathBuf>>>) -> Self {
        Self {
            inner,
            allowlist,
            inode_paths: RwLock::new(BTreeMap::new()),
        }
    }

    fn remember_inode_path(&self, inode: Inode, path: PathBuf) {
        self.inode_paths.write().unwrap().insert(inode, path);
    }

    fn inode_path(&self, inode: Inode) -> io::Result<PathBuf> {
        self.inode_paths
            .read()
            .unwrap()
            .get(&inode)
            .cloned()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))
    }

    fn rel_child_path(&self, parent: Inode, name: &CStr) -> io::Result<PathBuf> {
        let parent_path = self.inode_path(parent)?;
        let name = name
            .to_str()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        if name.contains('/') || name == "." || name == ".." {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(parent_path.join(name))
    }

    fn write_allowed_path(&self, path: &PathBuf) -> bool {
        self.allowlist.read().unwrap().iter().any(|allowed| {
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

    fn open_writes(flags: u32) -> bool {
        let flags = flags as i32;
        (flags & libc::O_ACCMODE) != libc::O_RDONLY || (flags & libc::O_TRUNC) != 0
    }
}

#[cfg(target_os = "macos")]
impl WritableAllowlistFs<PassthroughFs> {
    pub(crate) fn snapshot_state(&self) -> io::Result<PassthroughFsSnapshot> {
        self.inner.snapshot_state()
    }

    pub(crate) fn restore_state(&self, snap: &PassthroughFsSnapshot) -> io::Result<()> {
        self.inner.restore_state(snap)?;
        *self.inode_paths.write().unwrap() = snap
            .inode_paths()
            .iter()
            .map(|path| (path.inode, PathBuf::from(&path.path)))
            .collect();
        Ok(())
    }

    pub(crate) fn replay_dax_mappings(
        &self,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        self.inner.replay_dax_mappings(map_sender)
    }
}

impl<T: FileSystem<Inode = Inode, Handle = Handle>> FileSystem for WritableAllowlistFs<T> {
    type Inode = Inode;
    type Handle = Handle;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        self.remember_inode_path(fuse::ROOT_ID, PathBuf::new());
        self.inner.init(capable)
    }

    fn destroy(&self) {
        self.inode_paths.write().unwrap().clear();
        self.inner.destroy()
    }

    fn lookup(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        let path = self.rel_child_path(parent, name)?;
        let entry = self.inner.lookup(ctx, parent, name)?;
        self.remember_inode_path(entry.inode, path);
        Ok(entry)
    }

    fn forget(&self, ctx: Context, inode: Inode, count: u64) {
        self.inner.forget(ctx, inode, count);
        self.inode_paths.write().unwrap().remove(&inode);
    }

    fn batch_forget(&self, ctx: Context, requests: Vec<(Inode, u64)>) {
        self.inner.batch_forget(ctx, requests.clone());
        let mut paths = self.inode_paths.write().unwrap();
        for (inode, _) in requests {
            paths.remove(&inode);
        }
    }

    fn getattr(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Option<Handle>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.inner.getattr(ctx, inode, handle)
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
        }
        self.inner.setattr(ctx, inode, attr, handle, valid)
    }

    fn readlink(&self, ctx: Context, inode: Inode) -> io::Result<Vec<u8>> {
        self.inner.readlink(ctx, inode)
    }

    fn symlink(
        &self,
        ctx: Context,
        linkname: &CStr,
        parent: Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let path = self.check_write_child(parent, name)?;
        let entry = self
            .inner
            .symlink(ctx, linkname, parent, name, extensions)?;
        self.remember_inode_path(entry.inode, path);
        Ok(entry)
    }

    fn mknod(
        &self,
        ctx: Context,
        inode: Inode,
        name: &CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        let path = self.check_write_child(inode, name)?;
        let entry = self
            .inner
            .mknod(ctx, inode, name, mode, rdev, umask, extensions)?;
        self.remember_inode_path(entry.inode, path);
        Ok(entry)
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
        let path = self.check_write_child(parent, name)?;
        let entry = self
            .inner
            .mkdir(ctx, parent, name, mode, umask, extensions)?;
        self.remember_inode_path(entry.inode, path);
        Ok(entry)
    }

    fn unlink(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.check_write_child(parent, name)?;
        self.inner.unlink(ctx, parent, name)
    }

    fn rmdir(&self, ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.check_write_child(parent, name)?;
        self.inner.rmdir(ctx, parent, name)
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
        let old_path = self.check_write_child(olddir, oldname)?;
        let new_path = self.check_write_child(newdir, newname)?;
        self.inner
            .rename(ctx, olddir, oldname, newdir, newname, flags)?;
        let mut paths = self.inode_paths.write().unwrap();
        for path in paths.values_mut() {
            if *path == old_path {
                *path = new_path.clone();
            } else if let Ok(suffix) = path.strip_prefix(&old_path) {
                *path = new_path.join(suffix);
            }
        }
        Ok(())
    }

    fn link(
        &self,
        ctx: Context,
        inode: Inode,
        newparent: Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        let path = self.check_write_child(newparent, newname)?;
        let entry = self.inner.link(ctx, inode, newparent, newname)?;
        self.remember_inode_path(entry.inode, path);
        Ok(entry)
    }

    fn open(
        &self,
        ctx: Context,
        inode: Inode,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        if Self::open_writes(flags) {
            self.check_write_inode(inode)?;
        }
        self.inner.open(ctx, inode, kill_priv, flags)
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
        let path = self.check_write_child(parent, name)?;
        let (entry, handle, options) = self
            .inner
            .create(ctx, parent, name, mode, kill_priv, flags, umask, extensions)?;
        self.remember_inode_path(entry.inode, path);
        Ok((entry, handle, options))
    }

    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        w: W,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        self.inner
            .read(ctx, inode, handle, w, size, offset, lock_owner, flags)
    }

    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        r: R,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<usize> {
        self.check_write_inode(inode)?;
        self.inner.write(
            ctx,
            inode,
            handle,
            r,
            size,
            offset,
            lock_owner,
            delayed_write,
            kill_priv,
            flags,
        )
    }

    fn flush(&self, ctx: Context, inode: Inode, handle: Handle, lock_owner: u64) -> io::Result<()> {
        self.inner.flush(ctx, inode, handle, lock_owner)
    }

    fn fsync(&self, ctx: Context, inode: Inode, datasync: bool, handle: Handle) -> io::Result<()> {
        self.inner.fsync(ctx, inode, datasync, handle)
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
        self.inner
            .fallocate(ctx, inode, handle, mode, offset, length)
    }

    fn release(
        &self,
        ctx: Context,
        inode: Inode,
        flags: u32,
        handle: Handle,
        flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.inner
            .release(ctx, inode, flags, handle, flush, flock_release, lock_owner)
    }

    fn statfs(&self, ctx: Context, inode: Inode) -> io::Result<bindings::statvfs64> {
        self.inner.statfs(ctx, inode)
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
        self.inner.setxattr(ctx, inode, name, value, flags)
    }

    fn getxattr(
        &self,
        ctx: Context,
        inode: Inode,
        name: &CStr,
        size: u32,
    ) -> io::Result<GetxattrReply> {
        self.inner.getxattr(ctx, inode, name, size)
    }

    fn listxattr(&self, ctx: Context, inode: Inode, size: u32) -> io::Result<ListxattrReply> {
        self.inner.listxattr(ctx, inode, size)
    }

    fn removexattr(&self, ctx: Context, inode: Inode, name: &CStr) -> io::Result<()> {
        self.check_write_inode(inode)?;
        self.inner.removexattr(ctx, inode, name)
    }

    fn opendir(
        &self,
        ctx: Context,
        inode: Inode,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.inner.opendir(ctx, inode, flags)
    }

    fn readdir<F>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        self.inner
            .readdir(ctx, inode, handle, size, offset, add_entry)
    }

    fn readdirplus<F>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry, Entry) -> io::Result<usize>,
    {
        self.inner
            .readdirplus(ctx, inode, handle, size, offset, add_entry)
    }

    fn fsyncdir(
        &self,
        ctx: Context,
        inode: Inode,
        datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        self.inner.fsyncdir(ctx, inode, datasync, handle)
    }

    fn releasedir(&self, ctx: Context, inode: Inode, flags: u32, handle: Handle) -> io::Result<()> {
        self.inner.releasedir(ctx, inode, flags, handle)
    }

    fn access(&self, ctx: Context, inode: Inode, mask: u32) -> io::Result<()> {
        if mask & (libc::W_OK as u32) != 0 {
            self.check_write_inode(inode)?;
        }
        self.inner.access(ctx, inode, mask)
    }

    fn lseek(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        self.inner.lseek(ctx, inode, handle, offset, whence)
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
        self.check_write_inode(inode_out)?;
        self.inner.copyfilerange(
            ctx, inode_in, handle_in, offset_in, inode_out, handle_out, offset_out, len, flags,
        )
    }

    fn setupmapping(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        host_shm_base: u64,
        shm_size: u64,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        if (flags & fuse::SetupmappingFlags::WRITE.bits()) != 0 {
            self.check_write_inode(inode)?;
        }
        self.inner.setupmapping(
            ctx,
            inode,
            handle,
            foffset,
            len,
            flags,
            moffset,
            host_shm_base,
            shm_size,
            #[cfg(target_os = "macos")]
            map_sender,
        )
    }

    fn removemapping(
        &self,
        ctx: Context,
        requests: Vec<fuse::RemovemappingOne>,
        host_shm_base: u64,
        shm_size: u64,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        self.inner.removemapping(
            ctx,
            requests,
            host_shm_base,
            shm_size,
            #[cfg(target_os = "macos")]
            map_sender,
        )
    }

    fn ioctl(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        flags: u32,
        cmd: u32,
        arg: u64,
        in_size: u32,
        out_size: u32,
        exit_code: &Arc<AtomicI32>,
    ) -> io::Result<Vec<u8>> {
        self.inner.ioctl(
            ctx, inode, handle, flags, cmd, arg, in_size, out_size, exit_code,
        )
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::ffi::CString;
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    use super::{Context, Extensions, FileSystem, FsOptions, WritableAllowlistFs, fuse};
    use crate::virtio::fs::bindings;
    use crate::virtio::fs::inode_alloc::InodeAllocator;
    use crate::virtio::fs::passthrough::{Config, PassthroughFs};

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("libkrun-policyfs-{name}-{}", std::process::id()));
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

    fn new_policy_fs(root: &Path, allowlist: Vec<PathBuf>) -> WritableAllowlistFs<PassthroughFs> {
        let inner = PassthroughFs::new(
            Config {
                root_dir: root.to_string_lossy().into_owned(),
                ..Default::default()
            },
            std::sync::Arc::new(InodeAllocator::new()),
        )
        .unwrap();
        let fs = WritableAllowlistFs::new(
            inner,
            std::sync::Arc::new(std::sync::RwLock::new(allowlist)),
        );
        fs.init(FsOptions::empty()).unwrap();
        fs
    }

    #[test]
    fn writable_allowlist_wrapper_rejects_sibling_creates() {
        let temp = TempRoot::new("policy");
        fs::create_dir(temp.path().join("allowed")).unwrap();
        fs::create_dir(temp.path().join("denied")).unwrap();
        let fs = new_policy_fs(temp.path(), vec![PathBuf::from("allowed")]);
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
    fn writable_allowlist_wrapper_allows_apfs_copy_range_and_hole_punch_in_allowed_path() {
        let temp = TempRoot::new("copy-range");
        let fs = new_policy_fs(temp.path(), vec![PathBuf::from(".")]);
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

        let copied = fs
            .copyfilerange(
                ctx,
                src_entry.inode,
                src_handle.unwrap(),
                0,
                dst_entry.inode,
                dst_handle.unwrap(),
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
        dst.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        let mut hole = [1u8; 4096];
        dst.read_exact(&mut hole).unwrap();
        assert_eq!(hole, [0u8; 4096]);
        dst.seek(SeekFrom::Start(2 * 1024 * 1024)).unwrap();
        dst.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"tail");

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
