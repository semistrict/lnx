package lnx

import (
	"log/slog"
	"net"

	"github.com/hugelgupf/p9/fsimpl/localfs"
	"github.com/hugelgupf/p9/p9"
)

// start9PServer starts a 9P2000.L file server on the given listener,
// serving rootPath with security filtering. Handles one client connection.
func start9PServer(listener net.Listener, rootPath string) {
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		slog.Debug("9p client connected")

		s := p9.NewServer(&filteredAttacher{inner: localfs.Attacher(rootPath)})
		s.Handle(conn, conn)
	}()
}

// start9PServerUnfiltered starts a 9P2000.L file server without security
// filtering. Used for CWD, extra shares, and ~/.lnx (all read-write).
func start9PServerUnfiltered(listener net.Listener, rootPath string) {
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		slog.Debug("9p unfiltered client connected", "root", rootPath)

		s := p9.NewServer(localfs.Attacher(rootPath))
		s.Handle(conn, conn)
	}()
}
