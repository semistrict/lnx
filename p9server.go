package lnx

import (
	"log/slog"
	"net"

	"github.com/hugelgupf/p9/fsimpl/localfs"
	"github.com/hugelgupf/p9/p9"
)

// start9PServer starts a 9P2000.L file server on the given listener,
// serving rootPath. The server handles one client connection (the guest).
func start9PServer(listener net.Listener, rootPath string) {
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		slog.Debug("9p client connected")

		s := p9.NewServer(localfs.Attacher(rootPath))
		s.Handle(conn, conn)
	}()
}
