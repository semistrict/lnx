package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"sort"
	"time"

	"github.com/semistrict/lnx"
	"github.com/spf13/cobra"
)

var portsCmd = &cobra.Command{
	Use:   "ports",
	Short: "Manage forwarded ports",
}

var portsListCmd = &cobra.Command{
	Use:   "list",
	Short: "List forwarded ports",
	RunE:  runPortsList,
}

func init() {
	portsCmd.AddCommand(portsListCmd)
	rootCmd.AddCommand(portsCmd)
}

func runPortsList(cmd *cobra.Command, args []string) error {
	resp, err := apiClient().Get("http://localhost/ports")
	if err != nil {
		if isNoVM(err) {
			fmt.Println("no VM running")
			return nil
		}
		return err
	}
	defer resp.Body.Close()

	var ports []lnx.PortEntry
	if err := json.NewDecoder(resp.Body).Decode(&ports); err != nil {
		return fmt.Errorf("read ports: %w", err)
	}

	if len(ports) == 0 {
		fmt.Println("no forwarded ports")
		return nil
	}

	sort.Slice(ports, func(i, j int) bool {
		return ports[i].Guest < ports[j].Guest
	})

	probe := &http.Client{Timeout: 500 * time.Millisecond}

	fmt.Printf("%-8s  %-8s  %s\n", "GUEST", "HOST", "URL")
	for _, p := range ports {
		url := ""
		if isHTTP(probe, p.Host) {
			url = fmt.Sprintf("http://localhost:%d", p.Host)
		}
		fmt.Printf("%-8d  %-8d  %s\n", p.Guest, p.Host, url)
	}
	return nil
}

func isHTTP(client *http.Client, port uint16) bool {
	resp, err := client.Head(fmt.Sprintf("http://localhost:%d/", port))
	if err != nil {
		return false
	}
	resp.Body.Close()
	return true
}
