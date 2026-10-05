//go:build darwin

package lnx

import (
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"

	"github.com/semistrict/lnx/internal/hv"
	"golang.org/x/sys/unix"
)

// Guest physical memory layout (matches QEMU ARM virt machine conventions).
const (
	gicDistBase   uint64 = 0x0800_0000
	gicRedistBase uint64 = 0x080A_0000
	hvRAMBase     uint64 = 0x4000_0000

	// ARM64 CPSR: EL1h with DAIF masked (D=1 A=1 I=1 F=1).
	pstateEL1h uint64 = 0x3C5

	// Note: HCR_EL2 is managed by Apple internally and cannot be read or
	// written via hv_vcpu_{get,set}_sys_reg. Apple sets TWI=1 (trap WFI),
	// TSC=1 (trap SMC), and IRQ/FIQ routing by default.

	// PSCI function IDs (SMC64 convention).
	psciVersion   uint64 = 0x8400_0000
	psciCPUOn64   uint64 = 0xC400_0003
	psciSystemOff uint64 = 0x8400_0008
	psciSystemRst uint64 = 0x8400_0009

	// PSCI return codes.
	psciSuccess       uint64 = 0
	psciNotSupported  uint64 = 0xFFFF_FFFF_FFFF_FFFF // -1 in unsigned
)

// Virtio MMIO layout (matches QEMU ARM virt machine).
const (
	virtioBase    uint64 = 0x0A00_0000
	virtioStride  uint64 = 0x200
	virtioIRQBase uint32 = 16
	virtioCount   uint   = 8 // slots available

	// PL011 UART (matches QEMU ARM virt machine).
	uartBase uint64 = 0x0900_0000
	uartSize uint64 = 0x1000
	uartIRQ  uint32 = 1 // SPI 1

	// SPI intids start at 32 in the GIC. DTB SPI numbers are offsets from 0.
	// hv_gic_set_spi takes the full intid, so SPI N in DTB = intid 32+N.
	spiBase uint32 = 32
)

// hvVM implements VirtualMachine using Apple Hypervisor.framework.
type hvVM struct {
	mem     []byte // host memory backing guest RAM
	memSize uint64
	cpus    uint
	layout  *hv.GuestLayout
	bus     hv.MMIOBus
	uart    *hv.PL011
	console *hv.VirtioConsole
	vsock   *hv.VirtioVsock
	net        *hv.VirtioNet
	transports []*hv.VirtioMMIO // all virtio transports for pending IRQ check

	// pendingSPIs is a bitmap of SPI intids (offset from spiBase) that
	// need edge-pulsing on the vCPU thread. Non-vCPU goroutines set
	// bits via atomic Or; drainPendingIRQs Swaps to 0 and edge-pulses
	// each set bit. This replaces a channel to avoid capacity limits
	// and deduplicate redundant IRQs during burst traffic.
	pendingSPIs atomic.Uint64

	// wakeCh is signaled by kick() to wake vCPUs sleeping in WFI.
	// Buffered so senders never block.
	wakeCh chan struct{}

	// gicSetSPI asserts/deasserts a GIC SPI. Defaults to hv.GICSetSPI;
	// overridden in tests to avoid requiring the HV framework.
	gicSetSPI func(uint32, bool) error

	vcpus      []*hv.VCPU
	vcpuWg     sync.WaitGroup // tracks running vCPU goroutines
	stateCh    chan VMState
	currentVCPU *hv.VCPU // set during MMIO handling for GIC redist

	mu       sync.Mutex
	stopped  bool
	stopOnce sync.Once
}

// buildHVVM creates a Hypervisor.framework-backed VM, ready to Start().
func buildHVVM(cfg *Config) (VirtualMachine, error) {
	memSize := cfg.memoryBytes()

	if err := hv.VMCreate(); err != nil {
		return nil, fmt.Errorf("hv vm create: %w", err)
	}

	// Allocate guest RAM via mmap (anonymous, private).
	mem, err := unix.Mmap(-1, 0, int(memSize),
		unix.PROT_READ|unix.PROT_WRITE, unix.MAP_ANON|unix.MAP_PRIVATE)
	if err != nil {
		hv.VMDestroy()
		return nil, fmt.Errorf("mmap guest ram: %w", err)
	}

	// Map into guest physical address space.
	if err := hv.VMMap(unsafe.Pointer(&mem[0]), hvRAMBase, memSize, hv.MemRWX); err != nil {
		unix.Munmap(mem)
		hv.VMDestroy()
		return nil, fmt.Errorf("hv vm map ram: %w", err)
	}

	// Set up the GIC (must happen before vCPU creation).
	gicCfg := hv.NewGICConfig()
	if err := gicCfg.SetDistributorBase(gicDistBase); err != nil {
		unix.Munmap(mem)
		hv.VMDestroy()
		return nil, fmt.Errorf("gic distributor base: %w", err)
	}
	if err := gicCfg.SetRedistributorBase(gicRedistBase); err != nil {
		unix.Munmap(mem)
		hv.VMDestroy()
		return nil, fmt.Errorf("gic redistributor base: %w", err)
	}
	if err := hv.GICCreate(gicCfg); err != nil {
		unix.Munmap(mem)
		hv.VMDestroy()
		return nil, fmt.Errorf("gic create: %w", err)
	}

	// Load kernel.
	kernelData, textOffset, err := hv.LoadKernel(cfg.KernelPath)
	if err != nil {
		unix.Munmap(mem)
		hv.VMDestroy()
		return nil, fmt.Errorf("load kernel: %w", err)
	}

	// Generate the initramfs containing the lnx guest init binary.
	// If InitBinary is empty (e.g. UART-only tests), boot without initramfs.
	var initrdData []byte
	if len(InitBinary) > 0 {
		initrdDir := filepath.Dir(cfg.RootfsPath)
		if cfg.InitramfsPath != "" {
			initrdDir = filepath.Dir(cfg.InitramfsPath)
		}
		initrdPath, err := writeInitramfs(initrdDir)
		if err != nil {
			unix.Munmap(mem)
			hv.VMDestroy()
			return nil, fmt.Errorf("write initramfs: %w", err)
		}
		initrdData, err = os.ReadFile(initrdPath)
		if err != nil {
			unix.Munmap(mem)
			hv.VMDestroy()
			return nil, fmt.Errorf("load initramfs: %w", err)
		}
		slog.Info("hv: loaded initramfs", "bytes", len(initrdData), "path", initrdPath)
	}

	// random.trust_cpu=on: trust Apple Silicon's hardware RNG (RNDR) to seed
	// the CRNG immediately. Without this, crypto/rand blocks in the guest
	// until enough entropy accumulates (no virtio-rng device in HV backend).
	// console=hvc0: use virtio-console (not PL011 UART) as the primary
	// console. PL011 polling generates constant MMIO exits that starve
	// the vtimer on single-CPU VMs. earlycon is kept for pre-driver output.
	cmdline := fmt.Sprintf("console=hvc0 earlycon=pl011,0x09000000 root=/dev/vda rw random.trust_cpu=on lnx.epoch=%d", epoch())
	if cfg.KernelArgs != "" {
		cmdline += " " + cfg.KernelArgs
	}

	// Generate DTB.
	dtbData := hv.BuildVirtDTB(hv.VirtDTBConfig{
		RAMBase:       hvRAMBase,
		RAMSize:       memSize,
		CPUs:          cfg.cpus(),
		Cmdline:       cmdline,
		GICDistBase:   gicDistBase,
		GICDistSize:   0x10000,
		GICRedistBase: gicRedistBase,
		GICRedistSize: 0xF60000,
		UARTBase:      uartBase,
		UARTIRQ:       uartIRQ,
		VirtioBase:    virtioBase,
		VirtioStride:  virtioStride,
		VirtioCount:   virtioCount,
		VirtioIRQBase: virtioIRQBase,
	})

	// Place kernel, initrd, and DTB in guest memory.
	layout, err := hv.PlaceGuest(mem, hvRAMBase, kernelData, textOffset, initrdData, dtbData)
	if err != nil {
		unix.Munmap(mem)
		hv.VMDestroy()
		return nil, fmt.Errorf("place guest: %w", err)
	}
	// Re-generate DTB with initrd addresses now that we know them.
	if layout.InitrdAddr != 0 {
		dtbData = hv.BuildVirtDTB(hv.VirtDTBConfig{
			RAMBase:       hvRAMBase,
			RAMSize:       memSize,
			CPUs:          cfg.cpus(),
			Cmdline:       cmdline,
			GICDistBase:   gicDistBase,
			GICDistSize:   0x10000,
			GICRedistBase: gicRedistBase,
			GICRedistSize: 0xF60000,
			InitrdStart:   layout.InitrdAddr,
			InitrdEnd:     layout.InitrdEnd,
			UARTBase:      uartBase,
			UARTIRQ:       uartIRQ,
			VirtioBase:    virtioBase,
			VirtioStride:  virtioStride,
			VirtioCount:   virtioCount,
			VirtioIRQBase: virtioIRQBase,
		})
		copy(mem[0:], dtbData) // overwrite DTB at start of RAM
	}

	slog.Info("hv: guest layout",
		"kernel", fmt.Sprintf("0x%x", layout.KernelAddr),
		"dtb", fmt.Sprintf("0x%x", layout.DTBAddr),
		"initrd", fmt.Sprintf("0x%x-0x%x", layout.InitrdAddr, layout.InitrdEnd),
		"ram", fmt.Sprintf("0x%x+%dMB", layout.RAMBase, layout.RAMSize>>20))

	vm := &hvVM{
		mem:         mem,
		memSize:     memSize,
		cpus:        cfg.cpus(),
		layout:      layout,
		stateCh: make(chan VMState, 4),
		wakeCh:  make(chan struct{}, 1),
		gicSetSPI:   hv.GICSetSPI,
	}

	// irqAsync creates an irqFunc that calls hv_gic_set_spi directly.
	// Apple's docs: "You can call hv_gic_set_spi from any thread."
	// The GIC delivers the SPI to the running vCPU within hv_vcpu_run —
	// no ForceExit, no pendingSPIs bitmap, no race with SetPendingInterrupt.
	// Edge-pulse (deassert+assert) is needed because level-triggered SPIs
	// ignore set_spi(true) when already HIGH.
	irqAsync := func(intid uint32) func(bool) {
		return func(level bool) {
			if level {
				hv.GICSetSPI(intid, false) // deassert
				hv.GICSetSPI(intid, true)  // assert → GIC delivers to vCPU
				// Wake vCPU from WFI sleep.
				select {
				case vm.wakeCh <- struct{}{}:
				default:
				}
			} else {
				hv.GICSetSPI(intid, false)
			}
		}
	}

	// Set up PL011 UART for console I/O.
	// Uses irqAsync because QueueInput can be called from non-vCPU threads
	// (stdin reader, test harness).
	uartOut := cfg.uartWriter()
	vm.uart = hv.NewPL011(uartOut, irqAsync(spiBase+uartIRQ))
	vm.bus.Register(uartBase, uartSize, vm.uart)

	// Set up virtio-blk for rootfs (slot 0).
	rootfsFile, err := os.OpenFile(cfg.RootfsPath, os.O_RDWR, 0)
	if err != nil {
		unix.Munmap(mem)
		hv.VMDestroy()
		return nil, fmt.Errorf("open rootfs: %w", err)
	}
	blkDev, err := hv.NewVirtioBlk(rootfsFile)
	if err != nil {
		rootfsFile.Close()
		unix.Munmap(mem)
		hv.VMDestroy()
		return nil, fmt.Errorf("create virtio-blk: %w", err)
	}
	blkTransport := hv.NewVirtioMMIO(blkDev, mem, hvRAMBase, irqAsync(spiBase+virtioIRQBase+0))
	blkDev.SetTransport(blkTransport)
	vm.bus.Register(virtioBase, virtioStride, blkTransport)
	vm.transports = append(vm.transports, blkTransport)

	// Set up virtio-net for network access (slot 1) using the lnxnet userspace NAT.
	netBackend := newBridgeNetBackend()
	vm.net = hv.NewVirtioNet(netBackend)
	netTransport := hv.NewVirtioMMIO(vm.net, mem, hvRAMBase, irqAsync(spiBase+virtioIRQBase+1))
	vm.net.SetTransport(netTransport)
	vm.bus.Register(virtioBase+virtioStride, virtioStride, netTransport)
	vm.transports = append(vm.transports, netTransport)

	// Set up virtio-vsock for lnx host↔guest protocol (slot 2).
	vm.vsock = hv.NewVirtioVsock()
	vsockTransport := hv.NewVirtioMMIO(vm.vsock, mem, hvRAMBase, irqAsync(spiBase+virtioIRQBase+2))
	vm.vsock.SetTransport(vsockTransport)
	vm.bus.Register(virtioBase+2*virtioStride, virtioStride, vsockTransport)
	vm.transports = append(vm.transports, vsockTransport)

	// Set up virtio-console for hvc0 (slot 3). This replaces PL011 as the
	// primary console — PL011 polling generates constant MMIO exits that
	// starve the vtimer on single-CPU VMs.
	vm.console = hv.NewVirtioConsole(cfg.uartWriter())
	consoleTransport := hv.NewVirtioMMIO(vm.console, mem, hvRAMBase, irqAsync(spiBase+virtioIRQBase+3))
	vm.console.SetTransport(consoleTransport)
	vm.bus.Register(virtioBase+3*virtioStride, virtioStride, consoleTransport)
	vm.transports = append(vm.transports, consoleTransport)

	// Kick wakes the vCPU from WFI halt. GIC delivers SPIs to a
	// running vCPU automatically within hv_vcpu_run.
	kick := func() {
		select {
		case vm.wakeCh <- struct{}{}:
		default:
		}
	}
	netTransport.SetKickFn(kick)
	vsockTransport.SetKickFn(kick)
	consoleTransport.SetKickFn(kick)

	// Register GIC MMIO regions — forwarded to HV.framework's built-in GIC.
	vm.bus.Register(gicDistBase, 0x10000, hv.NewGICDistMMIO())
	// Redistributor is per-CPU; we register for CPU 0 and handle in the exit path.
	// With the framework's GIC, redistributor accesses on the vCPU thread use that vCPU.
	// We register the full redistributor region; the actual vCPU binding happens at dispatch time.
	vm.bus.Register(gicRedistBase, 0xF60000, &hvGICRedistForwarder{vm: vm})

	return vm, nil
}

func epoch() int64 {
	return time.Now().Unix()
}

func (h *hvVM) Start() error {
	h.stateCh <- VMStateStarting

	// Create vCPUs. Each runs in its own goroutine with LockOSThread.
	for i := uint(0); i < h.cpus; i++ {
		vcpuIdx := i
		errCh := make(chan error, 1)
		h.vcpuWg.Add(1)
		go func() {
			defer h.vcpuWg.Done()

			vcpu, err := hv.NewVCPU()
			if err != nil {
				errCh <- fmt.Errorf("vcpu %d create: %w", vcpuIdx, err)
				return
			}

			if err := h.initVCPU(vcpu, vcpuIdx); err != nil {
				vcpu.Destroy()
				errCh <- fmt.Errorf("vcpu %d init: %w", vcpuIdx, err)
				return
			}

			h.mu.Lock()
			h.vcpus = append(h.vcpus, vcpu)
			h.mu.Unlock()

			errCh <- nil

			// Run loop — blocks until VM exits.
			h.runVCPU(vcpu, vcpuIdx)
		}()

		if err := <-errCh; err != nil {
			h.cleanup()
			return err
		}
	}

	h.stateCh <- VMStateRunning

	// Forward host stdin to UART and virtio-console.
	go func() {
		buf := make([]byte, 256)
		for {
			n, err := os.Stdin.Read(buf)
			if n > 0 {
				h.uart.QueueInput(buf[:n])
				h.console.QueueInput(buf[:n])
				select {
				case h.wakeCh <- struct{}{}:
				default:
				}
				// UART/console IRQs are delivered via irqAsync → GIC.
				// Just wake the vCPU from WFI if halted.
			}
			if err != nil {
				return
			}
		}
	}()

	return nil
}

// initVCPU configures a freshly-created vCPU for boot.
func (h *hvVM) initVCPU(vcpu *hv.VCPU, idx uint) error {
	// MPIDR_EL1: Aff0 = vcpu index (GIC needs this for routing).
	if err := vcpu.SetSysReg(hv.SysRegMPIDR_EL1, uint64(idx)); err != nil {
		return fmt.Errorf("set MPIDR_EL1: %w", err)
	}

	if idx == 0 {
		// Boot CPU: set entry point and DTB address.
		// ARM64 boot protocol: x0 = DTB, x1-x3 = 0, PC = kernel entry.
		if err := vcpu.SetReg(hv.RegPC, h.layout.KernelAddr); err != nil {
			return fmt.Errorf("set PC: %w", err)
		}
		if err := vcpu.SetReg(hv.RegX0, h.layout.DTBAddr); err != nil {
			return fmt.Errorf("set X0 (DTB): %w", err)
		}
		if err := vcpu.SetReg(hv.RegCPSR, pstateEL1h); err != nil {
			return fmt.Errorf("set CPSR: %w", err)
		}
	}
	// Secondary CPUs stay halted until PSCI CPU_ON.

	return nil
}

// runVCPU is the vCPU execution loop. Runs on a locked OS thread.
func (h *hvVM) runVCPU(vcpu *hv.VCPU, idx uint) {
	defer vcpu.Destroy()

	// The HV framework automatically masks the VTimer on
	// HV_EXIT_REASON_VTIMER_ACTIVATED. We must explicitly unmask it
	// once the guest has reprogrammed the timer (CNTV_CTL_EL0 no longer
	// asserting). This matches QEMU's hvf_sync_vtimer pattern.
	vtimerMasked := false
	var exitCountException, exitCountVTimer, exitCountCanceled, exitCountWFI uint64

	// No periodic ticker. VCPUs are kicked via SIGUSR1 (Kick) which
	// only interrupts hv_vcpu_run, preserving SetPendingInterrupt.
	// Timer interrupts are injected by polling CVAL vs CNTVCT before
	// every hv_vcpu_run — no ExitVTimer needed.

	for {
		h.mu.Lock()
		if h.stopped {
			h.mu.Unlock()
			return
		}
		h.mu.Unlock()

		// Sync vtimer. The HV framework fires ExitVTimer on the
		// transition (timer newly asserting during hv_vcpu_run). If the
		// timer expires while we're in the exit handler, the transition
		// is missed. Check the timer state and inject if ISTATUS is set.
		if vtimerMasked {
			vtimerMasked = syncVTimer(vcpu)
			if vtimerMasked {
				vcpu.SetPendingInterrupt(hv.InterruptIRQ, true)
			}
		} else {
			// Check if the timer expired while we were in the exit
			// handler. The HV framework fires ExitVTimer on the
			// transition, but if the timer expires between hv_vcpu_run
			// calls, the transition is missed. Check CVAL vs CNTVCT
			// directly and inject the interrupt.
			ctl, _ := vcpu.GetSysReg(hv.SysRegCNTV_CTL_EL0)
			if ctl&0x3 == 0x1 { // enable=1, imask=0
				cval, _ := vcpu.GetSysReg(hv.SysRegCNTV_CVAL_EL0)
				off, _ := vcpu.GetVTimerOffset()
				cntvct := hv.MachAbsoluteTime() - off
				if cntvct >= cval {
					vcpu.SetPendingInterrupt(hv.InterruptIRQ, true)
				}
			}
		}

		exit, err := vcpu.Run()
		if err != nil {
			slog.Error("vcpu run failed", "vcpu", idx, "error", err)
			h.doStop()
			return
		}

		switch exit.Reason {
		case hv.ExitException:
			exitCountException++
			if vtimerMasked {
				vtimerMasked = syncVTimer(vcpu)
			}
			if exit.EC() == hv.ECWFx {
				exitCountWFI++
			}
			total := exitCountException + exitCountVTimer + exitCountCanceled
			if total%1000 == 0 {
				ctl, _ := vcpu.GetSysReg(hv.SysRegCNTV_CTL_EL0)
				cval, _ := vcpu.GetSysReg(hv.SysRegCNTV_CVAL_EL0)
				off, _ := vcpu.GetVTimerOffset()
				cntvct := hv.MachAbsoluteTime() - off
				var deltaMs int64
				if cval > cntvct {
					deltaMs = int64(cval-cntvct) / 24000 // 24MHz counter
				} else {
					deltaMs = -int64(cntvct-cval) / 24000
				}
				slog.Info("vcpu exit stats",
					"exception", exitCountException, "vtimer", exitCountVTimer,
					"canceled", exitCountCanceled, "wfi", exitCountWFI,
					"vtimerMasked", vtimerMasked,
					"ec", fmt.Sprintf("0x%02x", exit.EC()),
					"pa", fmt.Sprintf("0x%x", exit.PhysicalAddress),
					"cntv_ctl", fmt.Sprintf("0x%x", ctl),
					"timerDeltaMs", deltaMs)
			}
			if !h.handleException(vcpu, idx, exit) {
				return
			}
		case hv.ExitVTimer:
			exitCountVTimer++
			vtimerMasked = true
			vcpu.SetPendingInterrupt(hv.InterruptIRQ, true)
		case hv.ExitCanceled:
			exitCountCanceled++
			h.mu.Lock()
			stopping := h.stopped
			h.mu.Unlock()
			if stopping {
				return
			}
		default:
			slog.Error("unexpected exit reason", "vcpu", idx, "reason", exit.Reason)
			h.doStop()
			return
		}
	}
}

// handleException dispatches an ESR_EL2 exception. Returns false to stop the vCPU.
func (h *hvVM) handleException(vcpu hv.VCPUOps, idx uint, exit hv.ExitInfo) bool {
	switch exit.EC() {
	case hv.ECWFx:
		// WFI/WFE trapped — guest CPU is idle, waiting for an interrupt.
		advancePC(vcpu)

		// If any device has a pending interrupt, return immediately —
		// the pre-vcpu.Run SetPendingInterrupt will deliver it.
		for _, t := range h.transports {
			if t.HasPendingIRQ() {
				return true
			}
		}
		if h.pendingSPIs.Load() != 0 {
			return true
		}

		// No work pending. Wait for a kick or a short timeout, then
		// re-enter vcpu.Run so the kernel's programmed timer can fire
		// via ExitVTimer. Do NOT overwrite CNTV_CVAL — that destroys
		// the kernel's timer programming and prevents timers from
		// firing (the kernel's NO_HZ_IDLE subsystem sets CVAL for the
		// next event, and we must not clobber it).
		select {
		case <-h.wakeCh:
		case <-time.After(1 * time.Millisecond):
		}
		return true

	case hv.ECSMC64:
		return h.handlePSCI(vcpu, idx)

	case hv.ECDataAbort:
		h.handleMMIO(vcpu, idx, exit)
		return true

	case hv.ECSysReg:
		// Trapped MRS/MSR — decode and log. If this fires for ICC_* (GIC CPU
		// interface) registers, it means the HV framework isn't handling them
		// internally, which would break interrupt delivery.
		iss := exit.ISS()
		isRead := iss&1 == 1
		rt := (iss >> 5) & 0x1f
		op0 := (iss >> 20) & 3
		op1 := (iss >> 14) & 7
		crn := (iss >> 10) & 0xf
		crm := (iss >> 1) & 0xf
		op2 := (iss >> 17) & 7
		dir := "write"
		if isRead {
			dir = "read"
		}
		slog.Warn("trapped sysreg access", "vcpu", idx, dir, dir,
			"op0", op0, "op1", op1, "crn", crn, "crm", crm, "op2", op2,
			"rt", rt, "syndrome", fmt.Sprintf("0x%x", exit.Syndrome))
		advancePC(vcpu)
		return true

	default:
		slog.Error("unhandled exception", "vcpu", idx,
			"ec", fmt.Sprintf("0x%02x", exit.EC()),
			"syndrome", fmt.Sprintf("0x%x", exit.Syndrome))
		h.doStop()
		return false
	}
}

// handlePSCI processes a PSCI call via SMC. Returns false on SYSTEM_OFF/RESET.
func (h *hvVM) handlePSCI(vcpu hv.VCPUOps, idx uint) bool {
	fnID, _ := vcpu.GetReg(hv.RegX0)

	switch fnID {
	case psciVersion:
		// Return PSCI v1.0.
		vcpu.SetReg(hv.RegX0, 0x0001_0000)
		advancePC(vcpu)
		return true

	case psciSystemOff:
		slog.Info("guest requested system off", "vcpu", idx)
		h.doStop()
		return false

	case psciSystemRst:
		slog.Info("guest requested system reset", "vcpu", idx)
		h.doStop()
		return false

	case psciCPUOn64:
		// TODO: secondary CPU bringup.
		slog.Debug("PSCI CPU_ON not yet implemented", "vcpu", idx)
		vcpu.SetReg(hv.RegX0, psciNotSupported)
		advancePC(vcpu)
		return true

	default:
		slog.Debug("unknown PSCI function", "vcpu", idx,
			"fn", fmt.Sprintf("0x%x", fnID))
		vcpu.SetReg(hv.RegX0, psciNotSupported)
		advancePC(vcpu)
		return true
	}
}

// syncVTimer checks whether the guest has reprogrammed the virtual timer
// (CNTV_CTL_EL0 no longer asserting) and unmasks it if so. Returns true
// if the timer is still asserting (leave masked), false if unmasked.
// See QEMU target/arm/hvf/hvf.c hvf_sync_vtimer().
func syncVTimer(vcpu hv.VCPUOps) bool {
	ctl, err := vcpu.GetSysReg(hv.SysRegCNTV_CTL_EL0)
	if err != nil {
		return true // leave masked on error
	}
	const (
		tmrEnable  = 1 << 0
		tmrIMask   = 1 << 1
		tmrIStatus = 1 << 2
	)
	asserting := ctl&(tmrEnable|tmrIMask|tmrIStatus) == (tmrEnable | tmrIStatus)
	if asserting {
		return true // guest hasn't handled it yet; leave masked
	}
	vcpu.SetVTimerMask(false)
	return false
}

// handleMMIO dispatches a data abort to the appropriate MMIO device.
func (h *hvVM) handleMMIO(vcpu hv.VCPUOps, idx uint, exit hv.ExitInfo) {
	iss := exit.ISS()
	isv := (iss >> 24) & 1
	if isv == 0 {
		slog.Error("MMIO data abort without valid ISS", "vcpu", idx,
			"addr", fmt.Sprintf("0x%x", exit.PhysicalAddress))
		advancePC(vcpu)
		return
	}

	isWrite := (iss>>6)&1 == 1
	srt := (iss >> 16) & 0x1f
	sas := (iss >> 22) & 3
	size := uint32(1) << sas

	addr := exit.PhysicalAddress
	var val uint64

	if isWrite {
		val, _ = vcpu.GetReg(hv.RegXn(int(srt)))
	}

	// Store current vCPU for GIC redistributor forwarding.
	if realVCPU, ok := vcpu.(*hv.VCPU); ok {
		h.mu.Lock()
		h.currentVCPU = realVCPU
		h.mu.Unlock()
	}

	if !h.bus.Dispatch(addr, size, isWrite, &val) {
		if isWrite {
			slog.Debug("unhandled MMIO write", "vcpu", idx,
				"addr", fmt.Sprintf("0x%x", addr), "val", fmt.Sprintf("0x%x", val))
		} else {
			slog.Debug("unhandled MMIO read", "vcpu", idx,
				"addr", fmt.Sprintf("0x%x", addr))
		}
	}

	if !isWrite {
		vcpu.SetReg(hv.RegXn(int(srt)), val)
	}

	advancePC(vcpu)
}

// hvGICRedistForwarder forwards GIC redistributor MMIO to the current vCPU.
type hvGICRedistForwarder struct {
	vm *hvVM
}

func (g *hvGICRedistForwarder) Read(offset uint64, size uint32) uint64 {
	g.vm.mu.Lock()
	vcpu := g.vm.currentVCPU
	g.vm.mu.Unlock()
	if vcpu == nil {
		return 0
	}
	val, _ := hv.GICRedistRead(vcpu, offset)
	return val
}

func (g *hvGICRedistForwarder) Write(offset uint64, size uint32, val uint64) {
	g.vm.mu.Lock()
	vcpu := g.vm.currentVCPU
	g.vm.mu.Unlock()
	if vcpu == nil {
		return
	}
	hv.GICRedistWrite(vcpu, offset, val)
}

func (h *hvVM) Stop() error {
	h.doStop()
	return nil
}

func (h *hvVM) RequestStop() error {
	h.doStop()
	return nil
}

func (h *hvVM) doStop() {
	h.stopOnce.Do(func() {
		// Stop background device goroutines that access guest memory.
		if h.net != nil {
			h.net.Close()
		}

		h.mu.Lock()
		h.stopped = true
		for _, vcpu := range h.vcpus {
			vcpu.ForceExit()
		}
		h.mu.Unlock()

		// Wake any vCPU sleeping in WFI halt.
		select {
		case h.wakeCh <- struct{}{}:
		default:
		}

		// Wait for all vCPU goroutines to exit before releasing resources.
		h.vcpuWg.Wait()

		h.stateCh <- VMStateStopped

		// Release HV framework resources so another VM can be created.
		hv.VMUnmap(hvRAMBase, h.memSize)
		unix.Munmap(h.mem)
		hv.VMDestroy()
	})
}

func (h *hvVM) StateChangedNotify() <-chan VMState {
	return h.stateCh
}

func (h *hvVM) VsockDevice() VsockDevice {
	return h.vsock
}

func (h *hvVM) cleanup() {
	h.doStop()
}

// drainPendingIRQs atomically swaps the pending SPI bitmap and edge-pulses
// each set bit on the vCPU thread. Must be called from the vCPU thread
// because hv_gic_set_spi does NOT reliably propagate when called from
// non-vCPU threads.
//
// The edge pulse (deassert+assert) is needed because level-triggered SPIs
// ignore a set_spi(true) when already HIGH. The deassert forces the GIC
// to re-latch a new pending state on the subsequent assert.
//
// hv_gic_set_spi goes through Apple's built-in GIC, which automatically
// routes the interrupt to the vCPU. No SetPendingInterrupt needed — that
// API bypasses the GIC and is only for vtimer injection.
func (h *hvVM) drainPendingIRQs(vcpu hv.VCPUOps) {
	bits := h.pendingSPIs.Swap(0)
	if bits == 0 {
		return
	}
	for i := uint32(0); i < 64; i++ {
		if bits&(1<<i) != 0 {
			intid := spiBase + i
			h.gicSetSPI(intid, false)
			h.gicSetSPI(intid, true)
		}
	}
}

// advancePC moves the vCPU program counter past the current instruction (4 bytes for AArch64).
func advancePC(vcpu hv.VCPUOps) {
	pc, _ := vcpu.GetReg(hv.RegPC)
	vcpu.SetReg(hv.RegPC, pc+4)
}

