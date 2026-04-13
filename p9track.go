package lnx

import (
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/hugelgupf/p9/p9"
)

// shareWatcher pairs a share tag with its file tracker.
type shareWatcher struct {
	tag     string
	tracker *fileTracker
}

func cachePollInterval() time.Duration {
	return OptCachePoll.Get()
}

// trackedAttacher wraps a p9.Attacher and records which files the guest accesses.
type trackedAttacher struct {
	inner   p9.Attacher
	tracker *fileTracker
}

func (a *trackedAttacher) Attach() (p9.File, error) {
	f, err := a.inner.Attach()
	if err != nil {
		return nil, err
	}
	return &trackedFile{inner: f, relPath: "", tracker: a.tracker}, nil
}

// trackedFile wraps a p9.File and records paths on Walk/Open.
type trackedFile struct {
	inner   p9.File
	relPath string
	tracker *fileTracker
}

func (f *trackedFile) Walk(names []string) ([]p9.QID, p9.File, error) {
	qids, file, err := f.inner.Walk(names)
	if err != nil {
		return qids, file, err
	}
	newPath := f.relPath
	for _, name := range names {
		newPath = filepath.Join(newPath, name)
	}
	return qids, &trackedFile{inner: file, relPath: newPath, tracker: f.tracker}, nil
}

func (f *trackedFile) Open(mode p9.OpenFlags) (p9.QID, uint32, error) {
	qid, iounit, err := f.inner.Open(mode)
	if err == nil && f.relPath != "" {
		f.tracker.add(f.relPath)
	}
	return qid, iounit, err
}

func (f *trackedFile) WalkGetAttr(names []string) ([]p9.QID, p9.File, p9.AttrMask, p9.Attr, error) {
	qids, file, mask, attr, err := f.inner.WalkGetAttr(names)
	if err != nil {
		return qids, file, mask, attr, err
	}
	newPath := f.relPath
	for _, name := range names {
		newPath = filepath.Join(newPath, name)
	}
	return qids, &trackedFile{inner: file, relPath: newPath, tracker: f.tracker}, mask, attr, nil
}

func (f *trackedFile) Readdir(offset uint64, count uint32) (p9.Dirents, error) {
	if f.relPath != "" {
		f.tracker.add(f.relPath)
	}
	return f.inner.Readdir(offset, count)
}

// Delegate everything else.

func (f *trackedFile) StatFS() (p9.FSStat, error) { return f.inner.StatFS() }
func (f *trackedFile) GetAttr(req p9.AttrMask) (p9.QID, p9.AttrMask, p9.Attr, error) {
	return f.inner.GetAttr(req)
}
func (f *trackedFile) SetAttr(valid p9.SetAttrMask, attr p9.SetAttr) error {
	return f.inner.SetAttr(valid, attr)
}
func (f *trackedFile) Close() error                                   { return f.inner.Close() }
func (f *trackedFile) ReadAt(p []byte, offset int64) (int, error)     { return f.inner.ReadAt(p, offset) }
func (f *trackedFile) WriteAt(p []byte, offset int64) (int, error)    { return f.inner.WriteAt(p, offset) }
func (f *trackedFile) FSync() error                                   { return f.inner.FSync() }
func (f *trackedFile) Lock(pid int, lt p9.LockType, flags p9.LockFlags, start, length uint64, client string) (p9.LockStatus, error) {
	return f.inner.Lock(pid, lt, flags, start, length, client)
}
func (f *trackedFile) Create(name string, flags p9.OpenFlags, perm p9.FileMode, uid p9.UID, gid p9.GID) (p9.File, p9.QID, uint32, error) {
	return f.inner.Create(name, flags, perm, uid, gid)
}
func (f *trackedFile) Mkdir(name string, perm p9.FileMode, uid p9.UID, gid p9.GID) (p9.QID, error) {
	return f.inner.Mkdir(name, perm, uid, gid)
}
func (f *trackedFile) Symlink(oldName, newName string, uid p9.UID, gid p9.GID) (p9.QID, error) {
	return f.inner.Symlink(oldName, newName, uid, gid)
}
func (f *trackedFile) Link(target p9.File, newName string) error {
	return f.inner.Link(target, newName)
}
func (f *trackedFile) Mknod(name string, mode p9.FileMode, major, minor uint32, uid p9.UID, gid p9.GID) (p9.QID, error) {
	return f.inner.Mknod(name, mode, major, minor, uid, gid)
}
func (f *trackedFile) Rename(newDir p9.File, newName string) error {
	return f.inner.Rename(newDir, newName)
}
func (f *trackedFile) RenameAt(oldName string, newDir p9.File, newName string) error {
	return f.inner.RenameAt(oldName, newDir, newName)
}
func (f *trackedFile) UnlinkAt(name string, flags uint32) error {
	return f.inner.UnlinkAt(name, flags)
}
func (f *trackedFile) Readlink() (string, error)              { return f.inner.Readlink() }
func (f *trackedFile) Renamed(newDir p9.File, newName string) { f.inner.Renamed(newDir, newName) }
func (f *trackedFile) SetXattr(attr string, data []byte, flags p9.XattrFlags) error {
	return f.inner.SetXattr(attr, data, flags)
}
func (f *trackedFile) GetXattr(attr string) ([]byte, error) { return f.inner.GetXattr(attr) }
func (f *trackedFile) ListXattrs() ([]string, error)        { return f.inner.ListXattrs() }
func (f *trackedFile) RemoveXattr(attr string) error        { return f.inner.RemoveXattr(attr) }

// fileTracker records accessed paths and their host mtimes for change detection.
type fileTracker struct {
	rootPath string
	mu       sync.Mutex
	mtimes   map[string]syscall.Timespec // relative path -> mtime at last check
}

func newFileTracker(rootPath string) *fileTracker {
	return &fileTracker{rootPath: rootPath, mtimes: make(map[string]syscall.Timespec)}
}

// add records a path as accessed. Captures its current mtime.
func (t *fileTracker) add(relPath string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if _, ok := t.mtimes[relPath]; ok {
		return
	}
	absPath := filepath.Join(t.rootPath, relPath)
	var st syscall.Stat_t
	if syscall.Lstat(absPath, &st) == nil {
		t.mtimes[relPath] = statMtime(&st)
	}
}

func timespecEqual(a, b syscall.Timespec) bool {
	return a.Sec == b.Sec && a.Nsec == b.Nsec
}

// scanDir checks tracked paths under the given directory (relative to rootPath)
// for mtime changes. Called when FSEvents reports a directory-level change.
// Returns changed relative paths and updates stored mtimes.
func (t *fileTracker) scanDir(dir string) []string {
	t.mu.Lock()
	defer t.mu.Unlock()

	var changed []string
	for relPath, oldMtime := range t.mtimes {
		// Only check files under the changed directory.
		relDir := filepath.Dir(relPath)
		if relDir != dir && !strings.HasPrefix(relDir, dir+"/") && dir != "." && dir != "" {
			continue
		}
		absPath := filepath.Join(t.rootPath, relPath)
		var st syscall.Stat_t
		if err := syscall.Lstat(absPath, &st); err != nil {
			slog.Debug("scanDir stat failed", "path", absPath, "error", err)
			if os.IsNotExist(err) {
				changed = append(changed, relPath)
				delete(t.mtimes, relPath)
			}
			continue
		}
		newMtime := statMtime(&st)
		if !timespecEqual(newMtime, oldMtime) {
			changed = append(changed, relPath)
			t.mtimes[relPath] = newMtime
		}
	}
	return changed
}
