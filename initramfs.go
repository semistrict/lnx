package lnx

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
)

// InitBinary must be set by the embedding binary (via go:embed).
// The library itself does not embed the init binary — the caller provides it.
var InitBinary []byte

// WriteInitramfsTo creates a cpio-format initramfs and is exported for testing.
func WriteInitramfsTo(dir string) (string, error) {
	return writeInitramfs(dir)
}

func writeInitramfs(dir string) (string, error) {
	if len(InitBinary) == 0 {
		return "", fmt.Errorf("lnx.InitBinary not set; embed the guest init binary and assign it")
	}

	path := filepath.Join(dir, "initramfs.cpio")

	var buf bytes.Buffer
	writeCpioEntry(&buf, "init", InitBinary, 0100755)
	writeCpioEntry(&buf, "TRAILER!!!", nil, 0)
	if pad := buf.Len() % 512; pad != 0 {
		buf.Write(make([]byte, 512-pad))
	}

	if err := os.WriteFile(path, buf.Bytes(), 0644); err != nil {
		return "", err
	}
	return path, nil
}

// writeCpioEntry writes a single entry in cpio "newc" format.
func writeCpioEntry(buf *bytes.Buffer, name string, data []byte, mode uint32) {
	nameBytes := append([]byte(name), 0)
	hdr := fmt.Sprintf(
		"070701"+
			"%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X",
		1,              // inode
		mode,           // mode
		0,              // uid
		0,              // gid
		1,              // nlink
		0,              // mtime
		len(data),      // filesize
		0,              // devmajor
		0,              // devminor
		0,              // rdevmajor
		0,              // rdevminor
		len(nameBytes), // namesize
		0,              // check
	)
	buf.WriteString(hdr)
	buf.Write(nameBytes)
	if hdrLen := len(hdr) + len(nameBytes); hdrLen%4 != 0 {
		buf.Write(make([]byte, 4-hdrLen%4))
	}
	if data != nil {
		buf.Write(data)
		if len(data)%4 != 0 {
			buf.Write(make([]byte, 4-len(data)%4))
		}
	}
}
