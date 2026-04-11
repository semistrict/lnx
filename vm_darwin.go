//go:build darwin

package lnx

import (
	"fmt"
	"time"

	vz "github.com/Code-Hex/vz/v3"
)

// darwinVM implements VirtualMachine using Apple Virtualization.framework.
type darwinVM struct {
	vm   *vz.VirtualMachine
	sock *vzVsockDevice
}

func (d *darwinVM) Start() error {
	return d.vm.Start()
}

func (d *darwinVM) Stop() error {
	return d.vm.Stop()
}

func (d *darwinVM) RequestStop() error {
	_, err := d.vm.RequestStop()
	return err
}

func (d *darwinVM) Pause() error {
	return d.vm.Pause()
}

func (d *darwinVM) Resume() error {
	return d.vm.Resume()
}

func (d *darwinVM) SaveState(path string) error {
	return d.vm.SaveMachineStateToPath(path)
}

func (d *darwinVM) RestoreState(path string) error {
	return d.vm.RestoreMachineStateFromURL(path)
}

func (d *darwinVM) StateChangedNotify() <-chan VMState {
	vzCh := d.vm.StateChangedNotify()
	ch := make(chan VMState, 1)
	go func() {
		defer close(ch)
		for state := range vzCh {
			ch <- vzStateToVMState(state)
		}
	}()
	return ch
}

func (d *darwinVM) VsockDevice() VsockDevice {
	return d.sock
}

func vzStateToVMState(s vz.VirtualMachineState) VMState {
	switch s {
	case vz.VirtualMachineStateStarting:
		return VMStateStarting
	case vz.VirtualMachineStateRunning, vz.VirtualMachineStateResuming:
		return VMStateRunning
	case vz.VirtualMachineStateStopped, vz.VirtualMachineStateStopping:
		return VMStateStopped
	case vz.VirtualMachineStatePaused, vz.VirtualMachineStatePausing,
		vz.VirtualMachineStateSaving, vz.VirtualMachineStateRestoring:
		return VMStatePaused
	default:
		return VMStateError
	}
}

// buildVM creates a Darwin VM configured and ready to start.
// macAddr is a stable MAC address string; if empty, one is generated.
// epoch overrides lnx.epoch in the kernel cmdline (0 = use current time).
func buildVM(cfg *Config, initrdPath, cwd, swapPath, homeDir, macAddr string, epoch int64) (VirtualMachine, error) {
	vmConfig, err := buildVMConfig(cfg, initrdPath, cwd, swapPath, homeDir, macAddr, epoch)
	if err != nil {
		return nil, err
	}

	vm, err := vz.NewVirtualMachine(vmConfig)
	if err != nil {
		return nil, fmt.Errorf("create vm: %w", err)
	}

	socketDevices := vm.SocketDevices()
	if len(socketDevices) == 0 {
		return nil, fmt.Errorf("no vsock devices")
	}

	return &darwinVM{
		vm:   vm,
		sock: &vzVsockDevice{dev: socketDevices[0]},
	}, nil
}

// buildVMConfig creates the VZ VM configuration.
func buildVMConfig(cfg *Config, initrdPath, cwd, swapPath, homeDir, macAddr string, epoch int64) (*vz.VirtualMachineConfiguration, error) {
	if epoch == 0 {
		epoch = time.Now().Unix()
	}
	cmdline := fmt.Sprintf("console=hvc0 lnx.epoch=%d resume=/dev/vdb", epoch)

	bootLoader, err := vz.NewLinuxBootLoader(
		cfg.KernelPath,
		vz.WithCommandLine(cmdline),
		vz.WithInitrd(initrdPath),
	)
	if err != nil {
		return nil, fmt.Errorf("boot loader: %w", err)
	}

	vmConfig, err := vz.NewVirtualMachineConfiguration(bootLoader, cfg.cpus(), cfg.memoryBytes())
	if err != nil {
		return nil, fmt.Errorf("vm config: %w", err)
	}

	if vz.IsNestedVirtualizationSupported() {
		platform, err := vz.NewGenericPlatformConfiguration()
		if err != nil {
			return nil, fmt.Errorf("platform config: %w", err)
		}
		if err := platform.SetNestedVirtualizationEnabled(true); err != nil {
			return nil, fmt.Errorf("enable nested virtualization: %w", err)
		}
		vmConfig.SetPlatformVirtualMachineConfiguration(platform)
	}

	// Note: CWD and extra shares are served via 9P over vsock (not VirtioFS)
	// so that the VM can hibernate — VirtioFS blocks kernel suspend.
	for _, attach := range []func(*vz.VirtualMachineConfiguration) error{
		func(c *vz.VirtualMachineConfiguration) error {
			return attachDisks(c, cfg.RootfsPath, swapPath, cfg.NestedRootfs)
		},
		func(c *vz.VirtualMachineConfiguration) error {
			return attachSerialConsole(c, cfg.socketDir())
		},
		func(c *vz.VirtualMachineConfiguration) error {
			return attachNetwork(c, macAddr)
		},
		attachMisc,
	} {
		if err := attach(vmConfig); err != nil {
			return nil, err
		}
	}

	vsockConfig, err := vz.NewVirtioSocketDeviceConfiguration()
	if err != nil {
		return nil, fmt.Errorf("vsock config: %w", err)
	}
	vmConfig.SetSocketDevicesVirtualMachineConfiguration([]vz.SocketDeviceConfiguration{vsockConfig})

	if ok, err := vmConfig.Validate(); !ok || err != nil {
		return nil, fmt.Errorf("validate config: %w", err)
	}

	return vmConfig, nil
}

// shutdownVM gracefully stops a Darwin VM.
func shutdownVM(vm VirtualMachine, exitCode int) {
	if exitCode == 130 {
		vm.Stop()
		return
	}

	vm.RequestStop()
	stateCh := vm.StateChangedNotify()
	select {
	case <-time.After(3 * time.Second):
		vm.Stop()
	case state := <-stateCh:
		if state != VMStateStopped {
			vm.Stop()
		}
	}
}
