//go:build linux

package main

import (
	"encoding/json"
	"log/slog"
	"os"

	"github.com/semistrict/lnx/internal/protocol"
)

const nestedDrivesPath = "/var/lib/lnx/nested-drives.json"

// writeNestedDrivesMapping writes the nested drives mapping to a well-known
// path so that nested lnx instances can discover their rootfs device.
func writeNestedDrivesMapping(setup *protocol.Setup) {
	if len(setup.NestedDrives) == 0 {
		return
	}

	os.MkdirAll("/var/lib/lnx", 0755)

	data, err := json.Marshal(setup.NestedDrives)
	if err != nil {
		slog.Warn("marshal nested drives", "error", err)
		return
	}
	if err := os.WriteFile(nestedDrivesPath, data, 0644); err != nil {
		slog.Warn("write nested drives mapping", "error", err)
		return
	}

	slog.Info("nested drives configured", "count", len(setup.NestedDrives))
}
