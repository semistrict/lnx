//go:build darwin

package lnx

import (
	"encoding/gob"
	"log/slog"
	"net"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/fsnotify/fsevents"
	"github.com/semistrict/lnx/internal/protocol"
)

// startInvalidationSender detects host-side file changes and pushes
// invalidation messages to the guest. Uses FSEvents when cache.fsevents=true,
// otherwise falls back to polling tracked files.
func startInvalidationSender(conn net.Conn, watchers []shareWatcher) {
	if OptCacheFSEvents.Get() {
		startInvalidationFSEvents(conn, watchers)
	} else {
		startInvalidationPoll(conn, watchers)
	}
}

func startInvalidationPoll(conn net.Conn, watchers []shareWatcher) {
	interval := cachePollInterval()
	slog.Info("invalidation sender started (poll)", "interval", interval)
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

func startInvalidationFSEvents(conn net.Conn, watchers []shareWatcher) {
	latency := OptCacheFSEventsLatency.Get()
	slog.Info("invalidation sender started (fsevents)", "latency", latency)
	enc := gob.NewEncoder(conn)

	type watchEntry struct {
		watcher shareWatcher
		absRoot string // cleaned absolute path with trailing /
	}
	var entries []watchEntry
	var paths []string

	for _, w := range watchers {
		abs, err := filepath.Abs(w.tracker.rootPath)
		if err != nil {
			slog.Warn("skip fsevents watcher", "path", w.tracker.rootPath, "error", err)
			continue
		}
		entries = append(entries, watchEntry{watcher: w, absRoot: filepath.Clean(abs) + "/"})
		paths = append(paths, filepath.Clean(abs))
	}

	// Longest prefix first so specific watchers (sync share) match before
	// broad ones (home dir).
	sort.Slice(entries, func(i, j int) bool {
		return len(entries[i].absRoot) > len(entries[j].absRoot)
	})

	if len(paths) == 0 {
		slog.Warn("no paths to watch")
		return
	}

	es := &fsevents.EventStream{
		Paths:   paths,
		Latency: latency,
		Flags:   fsevents.FileEvents,
	}
	if err := es.Start(); err != nil {
		slog.Warn("fsevents start failed, falling back to poll", "error", err)
		startInvalidationPoll(conn, watchers)
		return
	}
	defer es.Stop()

	for batch := range es.Events {
		changed := map[string][]string{} // tag -> relative paths
		for _, ev := range batch {
			absPath := ev.Path
			if !strings.HasPrefix(absPath, "/") {
				absPath = "/" + absPath
			}
			for _, e := range entries {
				if !strings.HasPrefix(absPath, e.absRoot) {
					continue
				}
				relPath := strings.TrimPrefix(absPath, e.absRoot)
				if relPath == "" {
					continue
				}
				relDir := filepath.Dir(relPath)
				for _, p := range e.watcher.tracker.scanDir(relDir) {
					changed[e.watcher.tag] = append(changed[e.watcher.tag], p)
				}
				break
			}
		}

		for tag, paths := range changed {
			seen := make(map[string]bool, len(paths))
			var unique []string
			for _, p := range paths {
				if !seen[p] {
					seen[p] = true
					unique = append(unique, p)
				}
			}
			slog.Debug("invalidating cached paths", "tag", tag, "count", len(unique))
			if err := enc.Encode(protocol.Invalidation{Tag: tag, Paths: unique}); err != nil {
				slog.Debug("invalidation sender stopped", "error", err)
				return
			}
		}
	}
}
