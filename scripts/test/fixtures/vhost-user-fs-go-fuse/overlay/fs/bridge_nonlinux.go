//go:build !linux

package fs

import "github.com/hanwen/go-fuse/v2/fuse"

func (b *rawBridge) Statx(cancel <-chan struct{}, in *fuse.StatxIn, out *fuse.StatxOut) fuse.Status {
	var attr fuse.AttrOut
	getattrIn := fuse.GetAttrIn{
		InHeader: in.InHeader,
		Flags_:   in.GetattrFlags,
		Fh_:      in.Fh,
	}
	status := b.GetAttr(cancel, &getattrIn, &attr)
	if status != fuse.OK {
		return status
	}

	out.AttrValid = attr.AttrValid
	out.AttrValidNsec = attr.AttrValidNsec
	out.Mask = in.SxMask
	out.Blksize = attr.Blksize
	out.Nlink = attr.Nlink
	out.Uid = attr.Uid
	out.Gid = attr.Gid
	out.Mode = uint16(attr.Mode)
	out.Ino = attr.Ino
	out.Size = attr.Size
	out.Blocks = attr.Blocks
	out.Atime = fuse.SxTime{Sec: attr.Atime, Nsec: attr.Atimensec}
	out.Ctime = fuse.SxTime{Sec: attr.Ctime, Nsec: attr.Ctimensec}
	out.Mtime = fuse.SxTime{Sec: attr.Mtime, Nsec: attr.Mtimensec}
	return fuse.OK
}
