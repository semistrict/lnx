package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"sort"
	"time"

	"github.com/charmbracelet/lipgloss/table"
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

type instancePort struct {
	Instance string
	Port     lnx.PortEntry
}

func runPortsList(cmd *cobra.Command, args []string) error {
	// If explicit --instance, show only that one.
	if instanceFlag {
		return runPortsListOne(instanceName)
	}
	return runPortsListAll()
}

func runPortsListAll() error {
	running := runningInstances()
	if len(running) == 0 {
		fmt.Println("no VM running")
		return nil
	}

	var all []instancePort
	for _, name := range running {
		ports, err := fetchPorts(name)
		if err != nil {
			continue
		}
		for _, p := range ports {
			all = append(all, instancePort{Instance: name, Port: p})
		}
	}

	if len(all) == 0 {
		fmt.Println("no forwarded ports")
		return nil
	}

	sort.Slice(all, func(i, j int) bool {
		if all[i].Instance != all[j].Instance {
			return all[i].Instance < all[j].Instance
		}
		return all[i].Port.Guest < all[j].Port.Guest
	})

	probe := &http.Client{Timeout: 500 * time.Millisecond}

	multiInstance := len(running) > 1
	var t *table.Table
	if multiInstance {
		t = newTable("INSTANCE", "GUEST", "HOST", "URL")
	} else {
		t = newTable("GUEST", "HOST", "URL")
	}

	for _, ip := range all {
		url := ""
		if isHTTP(probe, ip.Port.Host) {
			url = cyanStyle.Render(fmt.Sprintf("http://localhost:%d", ip.Port.Host))
		}
		guest := fmt.Sprintf("%d", ip.Port.Guest)
		host := fmt.Sprintf("%d", ip.Port.Host)
		if multiInstance {
			t.Row(ip.Instance, guest, host, url)
		} else {
			t.Row(guest, host, url)
		}
	}
	fmt.Println(t)
	return nil
}

func runPortsListOne(name string) error {
	ports, err := fetchPorts(name)
	if err != nil {
		if isNoVM(err) {
			fmt.Println("no VM running")
			return nil
		}
		return err
	}

	if len(ports) == 0 {
		fmt.Println("no forwarded ports")
		return nil
	}

	sort.Slice(ports, func(i, j int) bool {
		return ports[i].Guest < ports[j].Guest
	})

	probe := &http.Client{Timeout: 500 * time.Millisecond}

	t := newTable("GUEST", "HOST", "URL")
	for _, p := range ports {
		url := ""
		if isHTTP(probe, p.Host) {
			url = cyanStyle.Render(fmt.Sprintf("http://localhost:%d", p.Host))
		}
		t.Row(fmt.Sprintf("%d", p.Guest), fmt.Sprintf("%d", p.Host), url)
	}
	fmt.Println(t)
	return nil
}

func fetchPorts(name string) ([]lnx.PortEntry, error) {
	resp, err := apiClientFor(name).Get("http://localhost/ports")
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	var ports []lnx.PortEntry
	if err := json.NewDecoder(resp.Body).Decode(&ports); err != nil {
		return nil, fmt.Errorf("read ports: %w", err)
	}
	return ports, nil
}

func isHTTP(client *http.Client, port uint16) bool {
	resp, err := client.Head(fmt.Sprintf("http://localhost:%d/", port))
	if err != nil {
		return false
	}
	resp.Body.Close()
	return true
}
