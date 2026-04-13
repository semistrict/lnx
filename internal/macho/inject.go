// Package macho provides Mach-O binary manipulation for embedding data
// in named sections. It handles section resizing, segment shifting, and
// load command offset updates.
package macho

import (
	"encoding/binary"
	"fmt"
)

// Load command types.
const (
	lcCodeSignature     = 0x1D
	lcSegment64         = 0x19
	lcSymtab            = 0x2
	lcDysymtab          = 0xB
	lcDyldInfo          = 0x22
	lcDyldInfoOnly      = 0x80000022
	lcFunctionStarts    = 0x26
	lcDataInCode        = 0x29
	lcDylibCodeSignDrs  = 0x2B
	lcLinkerOptHint     = 0x2E
	lcDyldExportsTrie   = 0x80000033
	lcDyldChainedFixups = 0x80000034
)

// Structure field offsets.
const (
	headerSize = 32

	hdrMagicOff      = 0
	hdrNCmdsOff      = 16
	hdrSizeOfCmdsOff = 20

	lcCmdOff     = 0
	lcCmdsizeOff = 4

	segSegnameOff  = 8
	segVmaddrOff   = 24
	segVmsizeOff   = 32
	segFileoffOff  = 40
	segFilesizeOff = 48
	segNsectsOff   = 64
	segCmdSize     = 72

	sectSectnameOff = 0
	sectAddrOff     = 32
	sectSizeOff     = 40
	sectOffsetOff   = 48
	sectCmdSize     = 80

	symtabSymoffOff = 8
	symtabStroffOff = 16

	dysymtabTocoffOff         = 32
	dysymtabModtaboffOff      = 40
	dysymtabExtrefsymoffOff   = 48
	dysymtabIndirectsymoffOff = 56
	dysymtabExtreloffOff      = 64
	dysymtabLocreloffOff      = 72

	linkeditDataoffOff = 8

	dyldRebaseOffOff   = 8
	dyldBindOffOff     = 16
	dyldWeakBindOffOff = 24
	dyldLazyBindOffOff = 32
	dyldExportOffOff   = 40
)

const (
	blobAlignment = 16 * 1024
	magic64       = 0xFEEDFACF
)

func get32(data []byte, off int) uint32  { return binary.LittleEndian.Uint32(data[off:]) }
func set32(data []byte, off int, v uint32) { binary.LittleEndian.PutUint32(data[off:], v) }
func get64(data []byte, off int) uint64  { return binary.LittleEndian.Uint64(data[off:]) }
func set64(data []byte, off int, v uint64) { binary.LittleEndian.PutUint64(data[off:], v) }

func segname(data []byte, off int) string {
	name := data[off+segSegnameOff : off+segSegnameOff+16]
	for i, b := range name {
		if b == 0 {
			return string(name[:i])
		}
	}
	return string(name)
}

func sectname(data []byte, off int) string {
	name := data[off+sectSectnameOff : off+sectSectnameOff+16]
	for i, b := range name {
		if b == 0 {
			return string(name[:i])
		}
	}
	return string(name)
}

// AlignUp rounds size up to the next multiple of align.
func AlignUp(size, align uint64) uint64 {
	rem := size % align
	if rem == 0 {
		return size
	}
	return size + (align - rem)
}

// InjectSection replaces the content of the named section (segName, sectName)
// with blob, prepending a u64 length header. It shifts subsequent segments
// and updates all Mach-O offsets. Returns a valid unsigned Mach-O binary.
func InjectSection(src []byte, segName, sectName string, blob []byte) ([]byte, error) {
	if len(src) < headerSize {
		return nil, fmt.Errorf("binary too small")
	}
	if get32(src, hdrMagicOff) != magic64 {
		return nil, fmt.Errorf("not a 64-bit Mach-O")
	}

	ncmds := get32(src, hdrNCmdsOff)
	sizeOfCmds := get32(src, hdrSizeOfCmdsOff)

	type sectionHit struct {
		cmdOff, sectOff         int
		segFileoff, segFilesize uint64
		segVmaddr, segVmsize    uint64
	}
	var hit *sectionHit

	type seg struct {
		cmdOff                  int
		fileoff, vmaddr, vmsize uint64
		nsects                  uint32
	}
	var segs []seg

	off := headerSize
	endCmds := headerSize + int(sizeOfCmds)
	for i := uint32(0); i < ncmds; i++ {
		if off+8 > endCmds {
			return nil, fmt.Errorf("load commands truncated")
		}
		cmd := get32(src, off+lcCmdOff)
		cmdsize := get32(src, off+lcCmdsizeOff)

		if cmd == lcSegment64 {
			sn := segname(src, off)
			fo := get64(src, off+segFileoffOff)
			va := get64(src, off+segVmaddrOff)
			vs := get64(src, off+segVmsizeOff)
			fs := get64(src, off+segFilesizeOff)
			ns := get32(src, off+segNsectsOff)

			segs = append(segs, seg{off, fo, va, vs, ns})

			if sn == segName {
				sectBase := off + segCmdSize
				for s := uint32(0); s < ns; s++ {
					so := sectBase + int(s)*sectCmdSize
					if sectname(src, so) == sectName {
						hit = &sectionHit{off, so, fo, fs, va, vs}
						break
					}
				}
			}
		}
		off += int(cmdsize)
	}

	if hit == nil {
		return nil, fmt.Errorf("%s,%s section not found", segName, sectName)
	}

	dataHeaderSize := uint64(8)
	totalSize := dataHeaderSize + uint64(len(blob))
	alignedSize := AlignUp(totalSize, blobAlignment)
	origSegsize := hit.segFilesize
	sizeDiff := int64(alignedSize) - int64(origSegsize)

	outLen := int64(len(src)) + sizeDiff
	if outLen <= 0 {
		return nil, fmt.Errorf("invalid size after injection")
	}
	out := make([]byte, outLen)

	secStart := hit.segFileoff
	copy(out, src[:secStart])

	binary.LittleEndian.PutUint64(out[secStart:], uint64(len(blob)))
	copy(out[secStart+8:], blob)

	copy(out[secStart+alignedSize:], src[secStart+origSegsize:])

	newVmsize := AlignUp(alignedSize, blobAlignment)
	if newVmsize < 0x4000 {
		newVmsize = 0x4000
	}
	set64(out, hit.cmdOff+segFilesizeOff, alignedSize)
	set64(out, hit.cmdOff+segVmsizeOff, newVmsize)
	set64(out, hit.sectOff+sectSizeOff, totalSize)

	vmaddrDiff := int64(newVmsize) - int64(hit.segVmsize)
	for _, s := range segs {
		if s.fileoff <= secStart {
			continue
		}
		set64(out, s.cmdOff+segFileoffOff, uint64(int64(s.fileoff)+sizeDiff))
		if s.vmsize > 0 && s.vmaddr > 0 {
			set64(out, s.cmdOff+segVmaddrOff, uint64(int64(s.vmaddr)+vmaddrDiff))
		}
		sectBase := s.cmdOff + segCmdSize
		for i := uint32(0); i < s.nsects; i++ {
			so := sectBase + int(i)*sectCmdSize
			if v := get32(out, so+sectOffsetOff); v > 0 {
				set32(out, so+sectOffsetOff, uint32(int64(v)+sizeDiff))
			}
			if s.vmsize > 0 && s.vmaddr > 0 {
				if v := get64(out, so+sectAddrOff); v > 0 {
					set64(out, so+sectAddrOff, uint64(int64(v)+vmaddrDiff))
				}
			}
		}
	}

	off = headerSize
	for i := uint32(0); i < ncmds; i++ {
		cmd := get32(out, off+lcCmdOff)
		cmdsize := get32(out, off+lcCmdsizeOff)

		switch cmd {
		case lcSymtab:
			shiftAfter(out, off+symtabSymoffOff, secStart, sizeDiff)
			shiftAfter(out, off+symtabStroffOff, secStart, sizeDiff)
		case lcDysymtab:
			for _, f := range []int{
				dysymtabTocoffOff, dysymtabModtaboffOff, dysymtabExtrefsymoffOff,
				dysymtabIndirectsymoffOff, dysymtabExtreloffOff, dysymtabLocreloffOff,
			} {
				shiftAfter(out, off+f, secStart, sizeDiff)
			}
		case lcDyldChainedFixups, lcCodeSignature, lcFunctionStarts,
			lcDataInCode, lcDylibCodeSignDrs, lcLinkerOptHint, lcDyldExportsTrie:
			shiftAfter(out, off+linkeditDataoffOff, secStart, sizeDiff)
		case lcDyldInfo, lcDyldInfoOnly:
			for _, f := range []int{
				dyldRebaseOffOff, dyldBindOffOff, dyldWeakBindOffOff,
				dyldLazyBindOffOff, dyldExportOffOff,
			} {
				shiftAfter(out, off+f, secStart, sizeDiff)
			}
		}
		off += int(cmdsize)
	}

	return out, nil
}

func shiftAfter(data []byte, off int, threshold uint64, sizeDiff int64) {
	v := uint64(get32(data, off))
	if v == 0 || v < threshold {
		return
	}
	set32(data, off, uint32(int64(v)+sizeDiff))
}
