//go:build linux

package main

import (
	"context"
	"encoding/gob"
	"fmt"
	"hash/fnv"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/hanwen/go-fuse/v2/fs"
	"github.com/hanwen/go-fuse/v2/fuse"
	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
)

// stableIno returns a deterministic inode number for a relative path.
// This ensures that after the kernel FORGETs an inode and re-Lookups it,
// go-fuse returns the same FUSE node ID. Without this, getcwd() fails
// because the kernel's dentry tree points to stale node IDs.
func stableIno(path string) uint64 {
	h := fnv.New64a()
	h.Write([]byte(path))
	ino := h.Sum64()
	if ino == 0 {
		ino = 1 // 0 means auto-generate in go-fuse
	}
	return ino
}

// lazyCacheFS holds the lower (virtiofs, read-only) and cache (ext4, read-write) roots.
type lazyCacheFS struct {
	lower        string          // absolute path to virtiofs staging mount
	cache        string          // absolute path to ext4 cache directory
	blockedPaths map[string]bool // relative paths to block (nil = no filtering)
}

// isBlocked returns true if relPath (relative to the share root) is blocked.
// A path is blocked if it is in blockedPaths or is a descendant of one.
func (lfs *lazyCacheFS) isBlocked(relPath string) bool {
	if len(lfs.blockedPaths) == 0 {
		return false
	}
	if lfs.blockedPaths[relPath] {
		return true
	}
	for dir := relPath; dir != "." && dir != ""; dir = filepath.Dir(dir) {
		if lfs.blockedPaths[dir] {
			return true
		}
	}
	return false
}

// homeSyncBlockedPaths mirrors the blockedDirs set from p9filter.go on the host.
// These paths (relative to $HOME) are hidden from the guest to protect credentials.
var homeSyncBlockedPaths = map[string]bool{
	".ssh":    true,
	".gnupg":  true,
	".aws":    true,
	".docker": true,
	".kube":   true,
	// macOS Keychain
	"Library/Keychains": true,
	// Browser profiles
	"Library/Application Support/Google/Chrome":           true,
	"Library/Application Support/Google/Chrome Canary":    true,
	"Library/Application Support/Chromium":                true,
	"Library/Application Support/Firefox":                 true,
	"Library/Application Support/Microsoft Edge":          true,
	"Library/Application Support/BraveSoftware":           true,
	"Library/Application Support/Arc":                     true,
	"Library/Application Support/com.operasoftware.Opera": true,
	"Library/Safari":  true,
	"Library/Cookies": true,
	// Terraform state (may contain secrets)
	".terraform.d": true,
	// NPM tokens
	".npmrc": true,
	// 1Password CLI
	".op":        true,
	".1password": true,
	".config/op": true,
	"Library/Group Containers/2BUA8C4S2C.com.1password": true,
}

func (lfs *lazyCacheFS) lowerPath(rel string) string {
	if rel == "" {
		return lfs.lower
	}
	return filepath.Join(lfs.lower, rel)
}

func (lfs *lazyCacheFS) cachePath(rel string) string {
	if rel == "" {
		return lfs.cache
	}
	return filepath.Join(lfs.cache, rel)
}

// lazyCacheNode is a FUSE node in the lazy cache filesystem.
type lazyCacheNode struct {
	fs.Inode
	lfs  *lazyCacheFS
	root *lazyCacheNode // root node (self for the root)

	// per-node directory listing cache
	mu       sync.Mutex
	entries  []fuse.DirEntry
	lowerMod int64 // lower dir mtime when entries was last populated (-1 = invalid)
}

// relPath returns this node's path relative to the mount root, derived from
// the inode tree. This is always current even after renames.
func (n *lazyCacheNode) relPath() string {
	return n.Inode.Path(n.root.EmbeddedInode())
}

// Compile-time interface checks.
var (
	_ fs.NodeGetattrer  = (*lazyCacheNode)(nil)
	_ fs.NodeReaddirer  = (*lazyCacheNode)(nil)
	_ fs.NodeLookuper   = (*lazyCacheNode)(nil)
	_ fs.NodeOpener     = (*lazyCacheNode)(nil)
	_ fs.NodeCreater    = (*lazyCacheNode)(nil)
	_ fs.NodeMkdirer    = (*lazyCacheNode)(nil)
	_ fs.NodeUnlinker   = (*lazyCacheNode)(nil)
	_ fs.NodeRmdirer    = (*lazyCacheNode)(nil)
	_ fs.NodeRenamer    = (*lazyCacheNode)(nil)
	_ fs.NodeSymlinker  = (*lazyCacheNode)(nil)
	_ fs.NodeReadlinker = (*lazyCacheNode)(nil)
)

func (n *lazyCacheNode) lowerPath() string { return n.lfs.lowerPath(n.relPath()) }
func (n *lazyCacheNode) cachePath() string { return n.lfs.cachePath(n.relPath()) }

// Getattr returns attributes. Cache-first: if the file exists in the ext4
// cache, return cache attrs without hitting the virtiofs lower at all.
// The background refresh goroutine keeps the cache fresh.
func (n *lazyCacheNode) Getattr(ctx context.Context, fh fs.FileHandle, out *fuse.AttrOut) syscall.Errno {
	var st syscall.Stat_t
	if syscall.Lstat(n.cachePath(), &st) == nil {
		fuseAttrFromStat(&out.Attr, &st)
		return 0
	}
	if syscall.Lstat(n.lowerPath(), &st) == nil {
		fuseAttrFromStat(&out.Attr, &st)
		return 0
	}
	return syscall.ENOENT
}

// Lookup looks up a child node by name.
func (n *lazyCacheNode) Lookup(ctx context.Context, name string, out *fuse.EntryOut) (*fs.Inode, syscall.Errno) {
	childPath := filepath.Join(n.relPath(), name)
	if n.lfs.isBlocked(childPath) {
		return nil, syscall.EACCES
	}
	cp := n.lfs.cachePath(childPath)
	lp := n.lfs.lowerPath(childPath)

	// Cache-first: check ext4 cache before hitting 9P lower.
	var st syscall.Stat_t
	if err := syscall.Lstat(cp, &st); err != nil {
		if err2 := syscall.Lstat(lp, &st); err2 != nil {
			slog.Debug("lookup miss", "path", childPath, "cp", cp, "lp", lp)
			return nil, syscall.ENOENT
		}
	}

	fuseAttrFromStat(&out.Attr, &st)
	child := n.NewInode(ctx, &lazyCacheNode{lfs: n.lfs, root: n.root, lowerMod: -1}, fs.StableAttr{
		Mode: uint32(st.Mode & syscall.S_IFMT),
		Ino:  stableIno(childPath),
	})
	return child, 0
}

// Readdir returns directory entries, caching against lower mtime.
func (n *lazyCacheNode) Readdir(ctx context.Context) (fs.DirStream, syscall.Errno) {
	lp := n.lowerPath()
	cp := n.cachePath()

	var lst syscall.Stat_t
	lowerOk := syscall.Lstat(lp, &lst) == nil

	n.mu.Lock()
	defer n.mu.Unlock()

	if lowerOk && n.entries != nil && lst.Mtim.Sec == n.lowerMod {
		return fs.NewListDirStream(append([]fuse.DirEntry(nil), n.entries...)), 0
	}

	seen := make(map[string]bool)
	var entries []fuse.DirEntry

	if lowerOk {
		des, err := os.ReadDir(lp)
		if err == nil {
			for _, de := range des {
				childPath := filepath.Join(n.relPath(), de.Name())
				if n.lfs.isBlocked(childPath) {
					continue
				}
				var st syscall.Stat_t
				syscall.Lstat(filepath.Join(lp, de.Name()), &st)
				entries = append(entries, fuse.DirEntry{
					Name: de.Name(),
					Mode: uint32(st.Mode),
					Ino:  st.Ino,
				})
				seen[de.Name()] = true
			}
		}
	}

	// Include cache-only entries (guest-created files not present in lower).
	if cdes, err := os.ReadDir(cp); err == nil {
		for _, de := range cdes {
			if seen[de.Name()] {
				continue
			}
			childPath := filepath.Join(n.relPath(), de.Name())
			if n.lfs.isBlocked(childPath) {
				continue
			}
			var st syscall.Stat_t
			syscall.Lstat(filepath.Join(cp, de.Name()), &st)
			entries = append(entries, fuse.DirEntry{
				Name: de.Name(),
				Mode: uint32(st.Mode),
				Ino:  st.Ino,
			})
		}
	}

	if lowerOk {
		n.lowerMod = lst.Mtim.Sec
	}
	n.entries = entries
	return fs.NewListDirStream(append([]fuse.DirEntry(nil), entries...)), 0
}

// Open hydrates the file into cache if needed, then opens the cache copy.
func (n *lazyCacheNode) Open(ctx context.Context, flags uint32) (fs.FileHandle, uint32, syscall.Errno) {
	if err := n.hydrateFile(); err != nil {
		slog.Warn("sync cache: hydrate failed", "path", n.relPath(), "error", err)
		return nil, 0, syscall.EIO
	}
	cp := n.cachePath()
	// Strip O_CREAT/O_EXCL — Open is for existing files.
	openFlags := int(flags) &^ (syscall.O_CREAT | syscall.O_EXCL)
	f, err := os.OpenFile(cp, openFlags, 0)
	if err != nil {
		return nil, 0, fs.ToErrno(err)
	}
	rawFd, err := syscall.Dup(int(f.Fd()))
	f.Close()
	if err != nil {
		return nil, 0, fs.ToErrno(err)
	}
	return fs.NewLoopbackFile(rawFd), 0, 0
}

// Create creates a new file in the cache.
func (n *lazyCacheNode) Create(ctx context.Context, name string, flags uint32, mode uint32, out *fuse.EntryOut) (*fs.Inode, fs.FileHandle, uint32, syscall.Errno) {
	childPath := filepath.Join(n.relPath(), name)
	cp := n.lfs.cachePath(childPath)
	if err := os.MkdirAll(filepath.Dir(cp), 0755); err != nil {
		return nil, nil, 0, fs.ToErrno(err)
	}
	f, err := os.OpenFile(cp, int(flags)|syscall.O_CREAT, os.FileMode(mode))
	if err != nil {
		return nil, nil, 0, fs.ToErrno(err)
	}
	rawFd, dupErr := syscall.Dup(int(f.Fd()))
	var cst syscall.Stat_t
	syscall.Fstat(int(f.Fd()), &cst)
	f.Close()
	if dupErr != nil {
		return nil, nil, 0, fs.ToErrno(dupErr)
	}
	fuseAttrFromStat(&out.Attr, &cst)
	child := n.NewInode(ctx, &lazyCacheNode{lfs: n.lfs, root: n.root, lowerMod: -1}, fs.StableAttr{
		Mode: uint32(cst.Mode & syscall.S_IFMT),
	})
	return child, fs.NewLoopbackFile(rawFd), 0, 0
}

// Mkdir creates a directory in the cache.
func (n *lazyCacheNode) Mkdir(ctx context.Context, name string, mode uint32, out *fuse.EntryOut) (*fs.Inode, syscall.Errno) {
	childPath := filepath.Join(n.relPath(), name)
	cp := n.lfs.cachePath(childPath)
	if err := os.MkdirAll(cp, os.FileMode(mode)); err != nil {
		return nil, fs.ToErrno(err)
	}
	var cst syscall.Stat_t
	syscall.Lstat(cp, &cst)
	fuseAttrFromStat(&out.Attr, &cst)
	child := n.NewInode(ctx, &lazyCacheNode{lfs: n.lfs, root: n.root, lowerMod: -1}, fs.StableAttr{
		Mode: syscall.S_IFDIR,
		Ino:  stableIno(childPath),
	})
	return child, 0
}

// Unlink removes a file from the cache (lower is read-only).
func (n *lazyCacheNode) Unlink(ctx context.Context, name string) syscall.Errno {
	cp := n.lfs.cachePath(filepath.Join(n.relPath(), name))
	if err := os.Remove(cp); err != nil && !os.IsNotExist(err) {
		return fs.ToErrno(err)
	}
	return 0
}

// Rmdir removes a directory from the cache.
func (n *lazyCacheNode) Rmdir(ctx context.Context, name string) syscall.Errno {
	cp := n.lfs.cachePath(filepath.Join(n.relPath(), name))
	if err := os.Remove(cp); err != nil && !os.IsNotExist(err) {
		return fs.ToErrno(err)
	}
	return 0
}

// Rename renames within the cache and updates the child node's path
// so subsequent operations (Open, Getattr) reference the correct location.
func (n *lazyCacheNode) Rename(ctx context.Context, oldName string, newParent fs.InodeEmbedder, newName string, flags uint32) syscall.Errno {
	oldChildPath := filepath.Join(n.relPath(), oldName)
	var newParentPath string
	if np, ok := newParent.(*lazyCacheNode); ok {
		newParentPath = np.relPath()
	}
	newChildPath := filepath.Join(newParentPath, newName)

	oldCP := n.lfs.cachePath(oldChildPath)
	newCP := n.lfs.cachePath(newChildPath)
	slog.Debug("fuse rename", "oldCP", oldCP, "newCP", newCP)
	if err := os.MkdirAll(filepath.Dir(newCP), 0755); err != nil {
		return fs.ToErrno(err)
	}
	if err := os.Rename(oldCP, newCP); err != nil {
		slog.Warn("fuse rename failed", "error", err)
		return fs.ToErrno(err)
	}

	// Invalidate parent dir listing caches.
	n.mu.Lock()
	n.entries = nil
	n.mu.Unlock()
	if np, ok := newParent.(*lazyCacheNode); ok && np != n {
		np.mu.Lock()
		np.entries = nil
		np.mu.Unlock()
	}
	return 0
}

// Symlink creates a symlink in the cache.
func (n *lazyCacheNode) Symlink(ctx context.Context, target, name string, out *fuse.EntryOut) (*fs.Inode, syscall.Errno) {
	childPath := filepath.Join(n.relPath(), name)
	cp := n.lfs.cachePath(childPath)
	if err := os.MkdirAll(filepath.Dir(cp), 0755); err != nil {
		return nil, fs.ToErrno(err)
	}
	if err := os.Symlink(target, cp); err != nil {
		return nil, fs.ToErrno(err)
	}
	var cst syscall.Stat_t
	syscall.Lstat(cp, &cst)
	fuseAttrFromStat(&out.Attr, &cst)
	child := n.NewInode(ctx, &lazyCacheNode{lfs: n.lfs, root: n.root, lowerMod: -1}, fs.StableAttr{
		Mode: syscall.S_IFLNK,
		Ino:  stableIno(childPath),
	})
	return child, 0
}

// Readlink reads a symlink, checking cache then lower.
func (n *lazyCacheNode) Readlink(ctx context.Context) ([]byte, syscall.Errno) {
	if target, err := os.Readlink(n.cachePath()); err == nil {
		return []byte(target), 0
	}
	if target, err := os.Readlink(n.lowerPath()); err == nil {
		return []byte(target), 0
	}
	return nil, syscall.EINVAL
}

// hydrateFile copies a file from lower to cache if the cache is absent or stale.
func (n *lazyCacheNode) hydrateFile() error {
	lp := n.lowerPath()
	cp := n.cachePath()

	var lst syscall.Stat_t
	if err := syscall.Lstat(lp, &lst); err != nil {
		return nil // no lower file; caller opens a guest-created file
	}

	var cst syscall.Stat_t
	if syscall.Lstat(cp, &cst) == nil && cst.Mtim.Sec >= lst.Mtim.Sec {
		return nil // cache is current
	}

	return copyFileWithMtime(lp, cp, &lst)
}

// copyFileWithMtime copies src to dst, preserving permissions, ownership, and mtime.
func copyFileWithMtime(src, dst string, srcStat *syscall.Stat_t) error {
	if err := os.MkdirAll(filepath.Dir(dst), 0755); err != nil {
		return err
	}
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()

	out, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, os.FileMode(srcStat.Mode)&0777)
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	out.Close()

	// Preserve owner and mtime so freshness checks remain accurate.
	syscall.Lchown(dst, int(srcStat.Uid), int(srcStat.Gid))
	times := []syscall.Timespec{srcStat.Atim, srcStat.Mtim}
	syscall.UtimesNano(dst, times)
	return nil
}

// fuseAttrFromStat fills a fuse.Attr from a syscall.Stat_t.
func fuseAttrFromStat(attr *fuse.Attr, st *syscall.Stat_t) {
	attr.Ino = st.Ino
	attr.Size = uint64(st.Size)
	attr.Blocks = uint64(st.Blocks)
	attr.Atime = uint64(st.Atim.Sec)
	attr.Atimensec = uint32(st.Atim.Nsec)
	attr.Mtime = uint64(st.Mtim.Sec)
	attr.Mtimensec = uint32(st.Mtim.Nsec)
	attr.Ctime = uint64(st.Ctim.Sec)
	attr.Ctimensec = uint32(st.Ctim.Nsec)
	attr.Mode = uint32(st.Mode)
	attr.Nlink = uint32(st.Nlink)
	attr.Uid = st.Uid
	attr.Gid = st.Gid
	attr.Rdev = uint32(st.Rdev)
	attr.Blksize = uint32(st.Blksize)
}

// fuseCacheTimeout is the kernel-side cache TTL for entry and attribute lookups.
// Higher values reduce FUSE round-trips (the kernel serves from its own cache),
// but delay visibility of host-side changes by up to this duration.
var fuseCacheTimeout = 5 * time.Second

// cachedMounts maps share tags to their FUSE mount state so the
// invalidation receiver can evict cache entries and notify the kernel.
var (
	cachedMounts   = map[string]*cachedMount{}
	cachedMountsMu sync.Mutex
)

type cachedMount struct {
	lfs    *lazyCacheFS
	root   *lazyCacheNode
	server *fuse.Server
}

// startCachedMount mounts a FUSE lazy-cache filesystem for a single share.
// Called post-pivotRoot. The lower 9P and cache dirs must already exist
// (set up by mountCachedLower pre-pivotRoot).
func startCachedMount(guestPath, tag string, blocked map[string]bool) {
	lower := fmt.Sprintf("/var/lnx/lower/%s", tag)
	cache := fmt.Sprintf("/var/lnx/cache/%s", tag)

	lfs := &lazyCacheFS{lower: lower, cache: cache, blockedPaths: blocked}
	root := &lazyCacheNode{lfs: lfs, lowerMod: -1}
	root.root = root

	server, err := fs.Mount(guestPath, root, &fs.Options{
		MountOptions: fuse.MountOptions{
			AllowOther: true,
			FsName:     "lnx-sync",
			Name:       tag,
		},
		AttrTimeout:  &fuseCacheTimeout,
		EntryTimeout: &fuseCacheTimeout,
	})
	if err != nil {
		slog.Warn("cached mount failed", "path", guestPath, "tag", tag, "error", err)
		return
	}
	slog.Info("cached mount", "path", guestPath, "tag", tag)

	cachedMountsMu.Lock()
	cachedMounts[tag] = &cachedMount{lfs: lfs, root: root, server: server}
	cachedMountsMu.Unlock()

	go server.Wait()
}

// startInvalidationReceiver dials the host invalidation port and processes
// cache eviction messages. Each message lists paths in a share that changed
// on the host. The receiver deletes the cached copy and invalidates the
// kernel FUSE entry cache so the next access reads fresh data from 9P.
func startInvalidationReceiver() {
	conn, err := vsock.Dial(vsockHostCID, protocol.InvalidatePort, nil)
	if err != nil {
		slog.Warn("invalidation receiver dial failed", "error", err)
		return
	}
	slog.Info("invalidation receiver connected")

	dec := gob.NewDecoder(conn)
	for {
		var inv protocol.Invalidation
		if err := dec.Decode(&inv); err != nil {
			slog.Debug("invalidation receiver stopped", "error", err)
			return
		}

		cachedMountsMu.Lock()
		cm := cachedMounts[inv.Tag]
		cachedMountsMu.Unlock()
		if cm == nil {
			continue
		}

		for _, relPath := range inv.Paths {
			cp := cm.lfs.cachePath(relPath)
			if err := os.Remove(cp); err != nil && !os.IsNotExist(err) {
				slog.Warn("cache evict failed", "path", relPath, "error", err)
			}

			// Invalidate kernel FUSE entry cache so the next access
			// goes through Lookup/Getattr (which will miss cache and
			// read fresh data from the 9P lower).
			dir := filepath.Dir(relPath)
			name := filepath.Base(relPath)
			parent := cm.root.EmbeddedInode()
			if dir != "." && dir != "" {
				for _, part := range strings.Split(dir, "/") {
					child := parent.GetChild(part)
					if child == nil {
						parent = nil
						break
					}
					parent = child
				}
			}
			if parent != nil {
				parent.NotifyEntry(name)
			}
		}
	}
}
