//go:build linux

package main

import (
	"bufio"
	"encoding/gob"
	"log/slog"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"syscall"

	"github.com/mdlayher/vsock"
	"github.com/semistrict/lnx/internal/protocol"
)

// startStatusServer connects to the host on the status vsock port
// and serves StatusReq/StatusResp for the VM's lifetime.
func startStatusServer() {
	conn, err := vsock.Dial(vsockHostCID, protocol.StatusPort, nil)
	if err != nil {
		slog.Warn("status vsock dial failed", "error", err)
		return
	}
	slog.Info("guest status server connected", "vsock_port", protocol.StatusPort)

	go func() {
		defer conn.Close()
		enc := gob.NewEncoder(conn)
		dec := gob.NewDecoder(conn)

		for {
			var msg protocol.Msg
			if err := dec.Decode(&msg); err != nil {
				return
			}
			if msg.StatusReq == nil {
				continue
			}

			resp := gatherStatus(msg.StatusReq.IncludeDmesg)
			if err := enc.Encode(protocol.Msg{StatusResp: &resp}); err != nil {
				return
			}
		}
	}()
}

func gatherStatus(includeDmesg bool) protocol.StatusResp {
	resp := protocol.StatusResp{
		LoadAvg: readFileField("/proc/loadavg"),
	}

	// Uptime
	if fields := strings.Fields(readFileField("/proc/uptime")); len(fields) > 0 {
		resp.UptimeSecs, _ = strconv.ParseFloat(fields[0], 64)
	}

	// Memory
	meminfo := parseMeminfo()
	resp.MemTotalKB = meminfo["MemTotal"]
	resp.MemAvailKB = meminfo["MemAvailable"]
	resp.SwapTotalKB = meminfo["SwapTotal"]
	resp.SwapFreeKB = meminfo["SwapFree"]

	// Disk (rootfs at /)
	var stat syscall.Statfs_t
	if syscall.Statfs("/", &stat) == nil {
		resp.DiskTotalKB = stat.Blocks * uint64(stat.Bsize) / 1024
		resp.DiskUsedKB = (stat.Blocks - stat.Bfree) * uint64(stat.Bsize) / 1024
	}

	if includeDmesg {
		out, err := exec.Command("dmesg").Output()
		if err == nil {
			resp.Dmesg = string(out)
		}
	}

	return resp
}

func readFileField(path string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(data))
}

func parseMeminfo() map[string]uint64 {
	f, err := os.Open("/proc/meminfo")
	if err != nil {
		return nil
	}
	defer f.Close()

	result := map[string]uint64{}
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		line := scanner.Text()
		key, rest, ok := strings.Cut(line, ":")
		if !ok {
			continue
		}
		fields := strings.Fields(rest)
		if len(fields) == 0 {
			continue
		}
		val, err := strconv.ParseUint(fields[0], 10, 64)
		if err != nil {
			continue
		}
		result[key] = val
	}
	return result
}
