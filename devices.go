package lnx

import (
	"fmt"
	"os"

	vz "github.com/Code-Hex/vz/v3"
)

// attachSerial sets up the virtio serial console (hvc0) routed to /dev/null.
// Terminal I/O uses vsock instead; serial exists only for kernel boot messages.
func attachSerial(vmConfig *vz.VirtualMachineConfiguration) error {
	devNull, err := os.OpenFile("/dev/null", os.O_RDWR, 0)
	if err != nil {
		return fmt.Errorf("open /dev/null: %w", err)
	}
	attachment, err := vz.NewFileHandleSerialPortAttachment(devNull, devNull)
	if err != nil {
		devNull.Close()
		return fmt.Errorf("serial attachment: %w", err)
	}
	config, err := vz.NewVirtioConsoleDeviceSerialPortConfiguration(attachment)
	if err != nil {
		devNull.Close()
		return fmt.Errorf("serial config: %w", err)
	}
	vmConfig.SetSerialPortsVirtualMachineConfiguration([]*vz.VirtioConsoleDeviceSerialPortConfiguration{config})
	return nil
}

// attachDisks attaches the rootfs as /dev/vda and a swap image as /dev/vdb.
func attachDisks(vmConfig *vz.VirtualMachineConfiguration, rootfsPath, swapPath string) error {
	rootAttach, err := vz.NewDiskImageStorageDeviceAttachment(rootfsPath, false)
	if err != nil {
		return fmt.Errorf("root disk attachment: %w", err)
	}
	rootBlock, err := vz.NewVirtioBlockDeviceConfiguration(rootAttach)
	if err != nil {
		return fmt.Errorf("root block device: %w", err)
	}

	swapAttach, err := vz.NewDiskImageStorageDeviceAttachment(swapPath, false)
	if err != nil {
		return fmt.Errorf("swap disk attachment: %w", err)
	}
	swapBlock, err := vz.NewVirtioBlockDeviceConfiguration(swapAttach)
	if err != nil {
		return fmt.Errorf("swap block device: %w", err)
	}

	vmConfig.SetStorageDevicesVirtualMachineConfiguration([]vz.StorageDeviceConfiguration{rootBlock, swapBlock})
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

// attachGraphics adds a virtio-gpu device with a 1920x1200 scanout,
// plus USB keyboard and pointing device for input from the graphics window.
func attachGraphics(vmConfig *vz.VirtualMachineConfiguration) error {
	graphicsConfig, err := vz.NewVirtioGraphicsDeviceConfiguration()
	if err != nil {
		return fmt.Errorf("graphics config: %w", err)
	}
	scanout, err := vz.NewVirtioGraphicsScanoutConfiguration(2560, 1600)
	if err != nil {
		return fmt.Errorf("scanout config: %w", err)
	}
	graphicsConfig.SetScanouts(scanout)
	vmConfig.SetGraphicsDevicesVirtualMachineConfiguration([]vz.GraphicsDeviceConfiguration{graphicsConfig})

	keyboard, err := vz.NewUSBKeyboardConfiguration()
	if err != nil {
		return fmt.Errorf("keyboard config: %w", err)
	}
	vmConfig.SetKeyboardsVirtualMachineConfiguration([]vz.KeyboardConfiguration{keyboard})

	pointing, err := vz.NewUSBScreenCoordinatePointingDeviceConfiguration()
	if err != nil {
		return fmt.Errorf("pointing config: %w", err)
	}
	vmConfig.SetPointingDevicesVirtualMachineConfiguration([]vz.PointingDeviceConfiguration{pointing})

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
