package main

import (
	"encoding/json"
	"fmt"
	"sort"

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

	for _, p := range ports {
		if p.Guest == p.Host {
			fmt.Printf(":%d\n", p.Guest)
		} else {
			fmt.Printf(":%d -> :%d\n", p.Guest, p.Host)
		}
	}
	return nil
}
