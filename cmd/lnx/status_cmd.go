package main

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var showDmesg bool

var statusCmd = &cobra.Command{
	Use:   "status",
	Short: "Show status of running VM",
	RunE:  runStatus,
}

func init() {
	statusCmd.Flags().BoolVar(&showDmesg, "dmesg", false, "include kernel ring buffer")
	rootCmd.AddCommand(statusCmd)
}

func runStatus(cmd *cobra.Command, args []string) error {
	url := "http://localhost/status"
	if showDmesg {
		url += "?dmesg=1"
	}

	resp, err := apiClient().Get(url)
	if err != nil {
		if isNoVM(err) {
			fmt.Println("no VM running")
			return nil
		}
		return err
	}
	defer resp.Body.Close()

	var status lnx.StatusResponse
	if err := json.NewDecoder(resp.Body).Decode(&status); err != nil {
		return fmt.Errorf("read status: %w", err)
	}

	printStatus(&status)
	return nil
}

func printStatus(r *lnx.StatusResponse) {
	uptime := time.Duration(r.UptimeSecs * float64(time.Second))

	fmt.Printf("Command:  %s\n", strings.Join(r.Command, " "))
	fmt.Printf("User:     %s\n", r.User)
	fmt.Printf("Uptime:   %s\n", uptime.Truncate(time.Second))

	if r.MemTotalKB > 0 {
		memUsedMB := float64(r.MemTotalKB-r.MemAvailKB) / 1024
		memTotalMB := float64(r.MemTotalKB) / 1024
		pct := 0.0
		if r.MemTotalKB > 0 {
			pct = float64(r.MemTotalKB-r.MemAvailKB) * 100 / float64(r.MemTotalKB)
		}
		fmt.Printf("Memory:   %.1f / %.1f MB (%.0f%%)\n", memUsedMB, memTotalMB, pct)
	}

	if r.SwapTotalKB > 0 {
		swapUsedMB := float64(r.SwapTotalKB-r.SwapFreeKB) / 1024
		swapTotalMB := float64(r.SwapTotalKB) / 1024
		pct := float64(r.SwapTotalKB-r.SwapFreeKB) * 100 / float64(r.SwapTotalKB)
		fmt.Printf("Swap:     %.1f / %.1f MB (%.0f%%)\n", swapUsedMB, swapTotalMB, pct)
	}

	if r.DiskTotalKB > 0 {
		diskUsedGB := float64(r.DiskUsedKB) / 1024 / 1024
		diskTotalGB := float64(r.DiskTotalKB) / 1024 / 1024
		pct := float64(r.DiskUsedKB) * 100 / float64(r.DiskTotalKB)
		fmt.Printf("Disk:     %.1f / %.1f GB (%.0f%%)\n", diskUsedGB, diskTotalGB, pct)
	}

	if r.LoadAvg != "" {
		fmt.Printf("Load:     %s\n", r.LoadAvg)
	}

	if r.Dmesg != "" {
		fmt.Printf("\n--- dmesg ---\n%s", r.Dmesg)
	}
}
