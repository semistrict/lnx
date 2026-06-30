package fuse

import "syscall"

const (
	ENODATA = Status(syscall.ENODATA)
	ENOATTR = Status(syscall.ENOATTR)

	EREMOTEIO = Status(syscall.EIO)
)

type Attr struct {
	Ino  uint64
	Size uint64

	Blocks    uint64
	Atime     uint64
	Mtime     uint64
	Ctime     uint64
	Atimensec uint32
	Mtimensec uint32
	Ctimensec uint32
	Mode      uint32
	Nlink     uint32
	Owner
	Rdev uint32

	Blksize uint32
	Padding uint32
}

type SetAttrIn struct {
	SetAttrInCommon
}

type SetXAttrIn struct {
	InHeader
	Size  uint32
	Flags uint32
}

type GetXAttrIn struct {
	InHeader
	Size    uint32
	Padding uint32
}

const (
	CAP_NO_OPENDIR_SUPPORT  = (1 << 24)
	CAP_EXPLICIT_INVAL_DATA = (1 << 25)

	CAP_MAP_ALIGNMENT      = (1 << 26)
	CAP_SUBMOUNTS          = (1 << 27)
	CAP_HANDLE_KILLPRIV_V2 = (1 << 28)
	CAP_SETXATTR_EXT       = (1 << 29)
	CAP_INIT_EXT           = (1 << 30)
	CAP_INIT_RESERVED      = (1 << 31)

	CAP_RENAME_SWAP = 0x0
)

func (s *StatfsOut) FromStatfsT(statfs *syscall.Statfs_t) {
	s.Blocks = statfs.Blocks
	s.Bfree = statfs.Bfree
	s.Bavail = statfs.Bavail
	s.Files = statfs.Files
	s.Ffree = statfs.Ffree
	s.Bsize = uint32(statfs.Iosize)
	s.Frsize = s.Bsize

	if s.Bsize > statfs.Bsize {
		adj := uint64(s.Bsize / statfs.Bsize)
		s.Blocks /= adj
		s.Bfree /= adj
		s.Bavail /= adj
	}
}

func (o *InitOut) setFlags(flags uint64) {
	o.Flags = uint32(flags) | CAP_INIT_EXT
	o.Flags2 = uint32(flags >> 32)
}
