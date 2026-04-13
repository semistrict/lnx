//go:build linux

package lnx

import (
	"encoding/gob"
	"log/slog"
	"net"
	"time"

	"github.com/semistrict/lnx/internal/protocol"
)


// startInvalidationSender on Linux falls back to polling since FSEvents
// is macOS-only. Polls tracked files for mtime changes.
func startInvalidationSender(conn net.Conn, watchers []shareWatcher) {
	interval := cachePollInterval()
	slog.Info("invalidation sender started (poll)", "interval_ms", interval.Milliseconds())
	enc := gob.NewEncoder(conn)
	for {
		time.Sleep(interval)
		for _, w := range watchers {
			changed := w.tracker.scanDir(".")
			if len(changed) == 0 {
				continue
			}
			slog.Debug("invalidating cached paths", "tag", w.tag, "count", len(changed))
			if err := enc.Encode(protocol.Invalidation{Tag: w.tag, Paths: changed}); err != nil {
				slog.Debug("invalidation sender stopped", "error", err)
				return
			}
		}
	}
}
