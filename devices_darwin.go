//go:build darwin

package lnx

import (
	"fmt"
	"net"
	"path/filepath"

	vz "github.com/Code-Hex/vz/v3"
)

// attachDisks attaches the rootfs as /dev/vda, swap as /dev/vdb, and any
// nested instance rootfs files as /dev/vdc, /dev/vdd, etc.
func attachDisks(vmConfig *vz.VirtualMachineConfiguration, rootfsPath, swapPath string, nested []NestedRootfs) error {
	var devices []vz.StorageDeviceConfiguration

	rootAttach, err := vz.NewDiskImageStorageDeviceAttachment(rootfsPath, false)
	if err != nil {
		return fmt.Errorf("root disk attachment: %w", err)
	}
	rootBlock, err := vz.NewVirtioBlockDeviceConfiguration(rootAttach)
	if err != nil {
		return fmt.Errorf("root block device: %w", err)
	}
	devices = append(devices, rootBlock)

	swapAttach, err := vz.NewDiskImageStorageDeviceAttachment(swapPath, false)
	if err != nil {
		return fmt.Errorf("swap disk attachment: %w", err)
	}
	swapBlock, err := vz.NewVirtioBlockDeviceConfiguration(swapAttach)
	if err != nil {
		return fmt.Errorf("swap block device: %w", err)
	}
	devices = append(devices, swapBlock)

	// Nested instance rootfs drives.
	for _, nr := range nested {
		attach, err := vz.NewDiskImageStorageDeviceAttachment(nr.RootfsPath, false)
		if err != nil {
			return fmt.Errorf("nested disk %s attachment: %w", nr.InstanceName, err)
		}
		block, err := vz.NewVirtioBlockDeviceConfiguration(attach)
		if err != nil {
			return fmt.Errorf("nested disk %s block device: %w", nr.InstanceName, err)
		}
		devices = append(devices, block)
	}

	vmConfig.SetStorageDevicesVirtualMachineConfiguration(devices)
	return nil
}

// attachNetwork configures NAT networking with a stable MAC address.
// macAddr is a "xx:xx:xx:xx:xx:xx" string; if empty, a random one is generated.
func attachNetwork(vmConfig *vz.VirtualMachineConfiguration, macAddr string) error {
	natAttachment, err := vz.NewNATNetworkDeviceAttachment()
	if err != nil {
		return fmt.Errorf("nat attachment: %w", err)
	}
	netConfig, err := vz.NewVirtioNetworkDeviceConfiguration(natAttachment)
	if err != nil {
		return fmt.Errorf("network config: %w", err)
	}

	var mac *vz.MACAddress
	if macAddr != "" {
		hw, parseErr := net.ParseMAC(macAddr)
		if parseErr != nil {
			return fmt.Errorf("parse MAC %q: %w", macAddr, parseErr)
		}
		mac, err = vz.NewMACAddress(hw)
	} else {
		mac, err = vz.NewRandomLocallyAdministeredMACAddress()
	}
	if err != nil {
		return fmt.Errorf("MAC address: %w", err)
	}
	netConfig.SetMACAddress(mac)

	vmConfig.SetNetworkDevicesVirtualMachineConfiguration([]*vz.VirtioNetworkDeviceConfiguration{netConfig})
	return nil
}

func attachSerialConsole(vmConfig *vz.VirtualMachineConfiguration, logDir string) error {
	logPath := filepath.Join(logDir, "serial.log")
	attachment, err := vz.NewFileSerialPortAttachment(logPath, false)
	if err != nil {
		return fmt.Errorf("serial port attachment: %w", err)
	}
	serial, err := vz.NewVirtioConsoleDeviceSerialPortConfiguration(attachment)
	if err != nil {
		return fmt.Errorf("serial port config: %w", err)
	}
	vmConfig.SetSerialPortsVirtualMachineConfiguration([]*vz.VirtioConsoleDeviceSerialPortConfiguration{serial})
	return nil
}

func attachMisc(vmConfig *vz.VirtualMachineConfiguration) error {
	entropy, err := vz.NewVirtioEntropyDeviceConfiguration()
	if err != nil {
		return fmt.Errorf("entropy config: %w", err)
	}
	vmConfig.SetEntropyDevicesVirtualMachineConfiguration([]*vz.VirtioEntropyDeviceConfiguration{entropy})

	balloon, err := vz.NewVirtioTraditionalMemoryBalloonDeviceConfiguration()
	if err != nil {
		return fmt.Errorf("balloon config: %w", err)
	}
	vmConfig.SetMemoryBalloonDevicesVirtualMachineConfiguration([]vz.MemoryBalloonDeviceConfiguration{balloon})

	return nil
}
