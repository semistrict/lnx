package lnx

import (
	"path/filepath"

	"github.com/hugelgupf/p9/linux"
	"github.com/hugelgupf/p9/p9"
)

// blockedDirs are directory names under $HOME that are blocked from 9P access.
var blockedDirs = map[string]bool{
	// SSH keys
	".ssh": true,
	// GPG keys
	".gnupg": true,
	// AWS credentials
	".aws": true,
	// macOS Keychain
	"Library/Keychains": true,
	// Browser profiles (cookies, passwords, session tokens)
	"Library/Application Support/Google/Chrome":         true,
	"Library/Application Support/Google/Chrome Canary":  true,
	"Library/Application Support/Chromium":              true,
	"Library/Application Support/Firefox":               true,
	"Library/Application Support/Microsoft Edge":        true,
	"Library/Application Support/BraveSoftware":         true,
	"Library/Application Support/Arc":                   true,
	"Library/Application Support/com.operasoftware.Opera": true,
	"Library/Safari":     true,
	"Library/Cookies":    true,
	// Docker credentials
	".docker": true,
	// Kubernetes credentials
	".kube": true,
	// Terraform state (may contain secrets)
	".terraform.d": true,
	// NPM tokens
	".npmrc": true,
	// 1Password CLI
	".op":           true,
	".1password":    true,
	".config/op":    true,
	"Library/Group Containers/2BUA8C4S2C.com.1password": true,
}

// filteredAttacher wraps a p9.Attacher and filters sensitive directories.
type filteredAttacher struct {
	inner p9.Attacher
}

func (a *filteredAttacher) Attach() (p9.File, error) {
	f, err := a.inner.Attach()
	if err != nil {
		return nil, err
	}
	return &filteredFile{inner: f, relPath: ""}, nil
}

// filteredFile wraps a p9.File and blocks access to sensitive paths.
type filteredFile struct {
	inner   p9.File
	relPath string // path relative to the 9P root (home dir)
}

func (f *filteredFile) isBlocked(name string) bool {
	candidate := filepath.Join(f.relPath, name)
	if blockedDirs[candidate] {
		return true
	}
	// Check if the current path is inside a blocked tree.
	for dir := candidate; dir != "." && dir != ""; dir = filepath.Dir(dir) {
		if blockedDirs[dir] {
			return true
		}
	}
	return false
}

func (f *filteredFile) Walk(names []string) ([]p9.QID, p9.File, error) {
	for _, name := range names {
		if f.isBlocked(name) {
			return nil, nil, linux.EACCES
		}
	}
	qids, file, err := f.inner.Walk(names)
	if err != nil {
		return qids, file, err
	}
	newPath := f.relPath
	for _, name := range names {
		newPath = filepath.Join(newPath, name)
	}
	return qids, &filteredFile{inner: file, relPath: newPath}, nil
}

func (f *filteredFile) WalkGetAttr(names []string) ([]p9.QID, p9.File, p9.AttrMask, p9.Attr, error) {
	for _, name := range names {
		if f.isBlocked(name) {
			return nil, nil, p9.AttrMask{}, p9.Attr{}, linux.EACCES
		}
	}
	qids, file, mask, attr, err := f.inner.WalkGetAttr(names)
	if err != nil {
		return qids, file, mask, attr, err
	}
	newPath := f.relPath
	for _, name := range names {
		newPath = filepath.Join(newPath, name)
	}
	return qids, &filteredFile{inner: file, relPath: newPath}, mask, attr, nil
}

func (f *filteredFile) Readdir(offset uint64, count uint32) (p9.Dirents, error) {
	entries, err := f.inner.Readdir(offset, count)
	if err != nil {
		return entries, err
	}
	var filtered p9.Dirents
	for _, e := range entries {
		if !f.isBlocked(e.Name) {
			filtered = append(filtered, e)
		}
	}
	return filtered, nil
}

// Delegate everything else to inner.

func (f *filteredFile) StatFS() (p9.FSStat, error)                  { return f.inner.StatFS() }
func (f *filteredFile) GetAttr(req p9.AttrMask) (p9.QID, p9.AttrMask, p9.Attr, error) {
	return f.inner.GetAttr(req)
}
func (f *filteredFile) SetAttr(valid p9.SetAttrMask, attr p9.SetAttr) error {
	return f.inner.SetAttr(valid, attr)
}
func (f *filteredFile) Close() error                                           { return f.inner.Close() }
func (f *filteredFile) Open(mode p9.OpenFlags) (p9.QID, uint32, error)         { return f.inner.Open(mode) }
func (f *filteredFile) ReadAt(p []byte, offset int64) (int, error)             { return f.inner.ReadAt(p, offset) }
func (f *filteredFile) WriteAt(p []byte, offset int64) (int, error)            { return f.inner.WriteAt(p, offset) }
func (f *filteredFile) FSync() error                                           { return f.inner.FSync() }
func (f *filteredFile) Lock(pid int, lt p9.LockType, flags p9.LockFlags, start, length uint64, client string) (p9.LockStatus, error) {
	return f.inner.Lock(pid, lt, flags, start, length, client)
}
func (f *filteredFile) Create(name string, flags p9.OpenFlags, perm p9.FileMode, uid p9.UID, gid p9.GID) (p9.File, p9.QID, uint32, error) {
	return f.inner.Create(name, flags, perm, uid, gid)
}
func (f *filteredFile) Mkdir(name string, perm p9.FileMode, uid p9.UID, gid p9.GID) (p9.QID, error) {
	return f.inner.Mkdir(name, perm, uid, gid)
}
func (f *filteredFile) Symlink(oldName, newName string, uid p9.UID, gid p9.GID) (p9.QID, error) {
	return f.inner.Symlink(oldName, newName, uid, gid)
}
func (f *filteredFile) Link(target p9.File, newName string) error { return f.inner.Link(target, newName) }
func (f *filteredFile) Mknod(name string, mode p9.FileMode, major, minor uint32, uid p9.UID, gid p9.GID) (p9.QID, error) {
	return f.inner.Mknod(name, mode, major, minor, uid, gid)
}
func (f *filteredFile) Rename(newDir p9.File, newName string) error {
	return f.inner.Rename(newDir, newName)
}
func (f *filteredFile) RenameAt(oldName string, newDir p9.File, newName string) error {
	return f.inner.RenameAt(oldName, newDir, newName)
}
func (f *filteredFile) UnlinkAt(name string, flags uint32) error {
	return f.inner.UnlinkAt(name, flags)
}
func (f *filteredFile) Readlink() (string, error)       { return f.inner.Readlink() }
func (f *filteredFile) Renamed(newDir p9.File, newName string) { f.inner.Renamed(newDir, newName) }
func (f *filteredFile) SetXattr(attr string, data []byte, flags p9.XattrFlags) error {
	return f.inner.SetXattr(attr, data, flags)
}
func (f *filteredFile) GetXattr(attr string) ([]byte, error)   { return f.inner.GetXattr(attr) }
func (f *filteredFile) ListXattrs() ([]string, error)          { return f.inner.ListXattrs() }
func (f *filteredFile) RemoveXattr(attr string) error          { return f.inner.RemoveXattr(attr) }
