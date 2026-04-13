//go:build darwin

package lnx

import (
	"fmt"
	"path/filepath"

	vz "github.com/Code-Hex/vz/v3"
)

// attachDisks attaches block devices in order:
//
//	/dev/vda — rootfs
//	/dev/vdb — swap (hibernate resume device)
//	/dev/vdc — CRIU images volume
//	/dev/vdd, /dev/vde, ... — nested instance rootfs drives
func attachDisks(vmConfig *vz.VirtualMachineConfiguration, rootfsPath, swapPath, criuPath string, nested []NestedRootfs) error {
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

	criuAttach, err := vz.NewDiskImageStorageDeviceAttachment(criuPath, false)
	if err != nil {
		return fmt.Errorf("criu disk attachment: %w", err)
	}
	criuBlock, err := vz.NewVirtioBlockDeviceConfiguration(criuAttach)
	if err != nil {
		return fmt.Errorf("criu block device: %w", err)
	}
	devices = append(devices, criuBlock)

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

// All directory sharing now goes through 9P over vsock (no virtiofs).
// See setupVsock in vm.go for the 9P server setup.

func attachNetwork(vmConfig *vz.VirtualMachineConfiguration) error {
	natAttachment, err := vz.NewNATNetworkDeviceAttachment()
	if err != nil {
		return fmt.Errorf("nat attachment: %w", err)
	}
	netConfig, err := vz.NewVirtioNetworkDeviceConfiguration(natAttachment)
	if err != nil {
		return fmt.Errorf("network config: %w", err)
	}
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
