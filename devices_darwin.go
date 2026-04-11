//go:build darwin

package lnx

import (
	"fmt"
	"path/filepath"

	vz "github.com/Code-Hex/vz/v3"
)

// attachDisks attaches block devices in order:
//   /dev/vda — rootfs
//   /dev/vdb — swap (hibernate resume device)
//   /dev/vdc — CRIU images volume
//   /dev/vdd, /dev/vde, ... — nested instance rootfs drives
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

// attachShares mounts the CWD and extra share directories read-write via virtiofs.
// The home directory is served via 9P over vsock instead (for permission filtering).
func attachShares(vmConfig *vz.VirtualMachineConfiguration, cwd string, extraShares []string) error {
	var devices []vz.DirectorySharingDeviceConfiguration

	// CWD share.
	cwdFSConfig, err := vz.NewVirtioFileSystemDeviceConfiguration("cwd")
	if err != nil {
		return fmt.Errorf("cwd virtiofs config: %w", err)
	}
	cwdShared, err := vz.NewSharedDirectory(cwd, false)
	if err != nil {
		return fmt.Errorf("cwd shared dir: %w", err)
	}
	cwdShare, err := vz.NewSingleDirectoryShare(cwdShared)
	if err != nil {
		return fmt.Errorf("cwd dir share: %w", err)
	}
	cwdFSConfig.SetDirectoryShare(cwdShare)
	devices = append(devices, cwdFSConfig)

	// Extra shares.
	for i, path := range extraShares {
		tag := fmt.Sprintf("share%d", i)
		fsCfg, err := vz.NewVirtioFileSystemDeviceConfiguration(tag)
		if err != nil {
			return fmt.Errorf("share %s virtiofs config: %w", path, err)
		}
		shared, err := vz.NewSharedDirectory(path, false)
		if err != nil {
			return fmt.Errorf("share %s shared dir: %w", path, err)
		}
		share, err := vz.NewSingleDirectoryShare(shared)
		if err != nil {
			return fmt.Errorf("share %s dir share: %w", path, err)
		}
		fsCfg.SetDirectoryShare(share)
		devices = append(devices, fsCfg)
	}

	vmConfig.SetDirectorySharingDevicesVirtualMachineConfiguration(devices)
	return nil
}

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
