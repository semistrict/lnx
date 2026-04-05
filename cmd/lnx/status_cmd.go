package main

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/charmbracelet/lipgloss"
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
	// If no explicit --instance, show all running instances.
	if !instanceFlag {
		return runStatusAll()
	}
	return runStatusOne(instanceName)
}

var instanceHeader = lipgloss.NewStyle().
	Bold(true).
	Foreground(lipgloss.Color("6")).
	BorderStyle(lipgloss.NormalBorder()).
	BorderBottom(true).
	BorderForeground(lipgloss.Color("8")).
	MarginBottom(1)

func runStatusAll() error {
	running := runningInstances()
	if len(running) == 0 {
		fmt.Println("no VM running")
		return nil
	}

	for i, name := range running {
		if i > 0 {
			fmt.Println()
		}
		if len(running) > 1 {
			fmt.Println(instanceHeader.Render(name))
		}
		if err := runStatusOne(name); err != nil {
			fmt.Printf("  error: %v\n", err)
		}
	}
	return nil
}

func runStatusOne(name string) error {
	url := "http://localhost/status"
	if showDmesg {
		url += "?dmesg=1"
	}

	resp, err := apiClientFor(name).Get(url)
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

	kv := func(label, value string) {
		fmt.Printf("%s  %s\n", labelStyle.Width(10).Align(lipgloss.Right).Render(label), valueStyle.Render(value))
	}

	kv("Command", strings.Join(r.Command, " "))
	kv("User", r.User)
	kv("Uptime", uptime.Truncate(time.Second).String())

	if r.MemTotalKB > 0 {
		memUsedMB := float64(r.MemTotalKB-r.MemAvailKB) / 1024
		memTotalMB := float64(r.MemTotalKB) / 1024
		pct := 0.0
		if r.MemTotalKB > 0 {
			pct = float64(r.MemTotalKB-r.MemAvailKB) * 100 / float64(r.MemTotalKB)
		}
		kv("Memory", fmt.Sprintf("%.1f / %.1f MB (%.0f%%)", memUsedMB, memTotalMB, pct))
	}

	if r.SwapTotalKB > 0 {
		swapUsedMB := float64(r.SwapTotalKB-r.SwapFreeKB) / 1024
		swapTotalMB := float64(r.SwapTotalKB) / 1024
		pct := float64(r.SwapTotalKB-r.SwapFreeKB) * 100 / float64(r.SwapTotalKB)
		kv("Swap", fmt.Sprintf("%.1f / %.1f MB (%.0f%%)", swapUsedMB, swapTotalMB, pct))
	}

	if r.DiskTotalKB > 0 {
		diskUsedGB := float64(r.DiskUsedKB) / 1024 / 1024
		diskTotalGB := float64(r.DiskTotalKB) / 1024 / 1024
		pct := float64(r.DiskUsedKB) * 100 / float64(r.DiskTotalKB)
		kv("Disk", fmt.Sprintf("%.1f / %.1f GB (%.0f%%)", diskUsedGB, diskTotalGB, pct))
	}

	if r.LoadAvg != "" {
		kv("Load", r.LoadAvg)
	}

	if r.Dmesg != "" {
		fmt.Printf("\n%s\n%s", dimStyle.Render("--- dmesg ---"), r.Dmesg)
	}
}
