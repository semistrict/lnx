//go:build darwin

package lnx

import (
	"sync"
	"testing"

	"github.com/semistrict/lnx/internal/hv"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// fakeVCPU implements hv.VCPUOps for unit testing VMM handlers without
// the Hypervisor framework.
type fakeVCPU struct {
	regs    map[hv.Reg]uint64
	sysRegs map[hv.SysReg]uint64

	mu              sync.Mutex
	vtimerMaskCalls []bool
}

func newFakeVCPU() *fakeVCPU {
	return &fakeVCPU{
		regs:    make(map[hv.Reg]uint64),
		sysRegs: make(map[hv.SysReg]uint64),
	}
}

func (f *fakeVCPU) GetReg(r hv.Reg) (uint64, error)    { return f.regs[r], nil }
func (f *fakeVCPU) SetReg(r hv.Reg, v uint64) error     { f.regs[r] = v; return nil }
func (f *fakeVCPU) GetSysReg(r hv.SysReg) (uint64, error) { return f.sysRegs[r], nil }
func (f *fakeVCPU) SetSysReg(r hv.SysReg, v uint64) error { f.sysRegs[r] = v; return nil }
func (f *fakeVCPU) SetPendingInterrupt(hv.InterruptType, bool) error { return nil }
func (f *fakeVCPU) GetVTimerOffset() (uint64, error)                { return 0, nil }
func (f *fakeVCPU) SetVTimerMask(masked bool) error {
	f.mu.Lock()
	f.vtimerMaskCalls = append(f.vtimerMaskCalls, masked)
	f.mu.Unlock()
	return nil
}

// gicRecorder records GICSetSPI calls for assertions.
type gicRecorder struct {
	mu    sync.Mutex
	calls []gicCall
}
type gicCall struct {
	intid uint32
	level bool
}

func (g *gicRecorder) set(intid uint32, level bool) error {
	g.mu.Lock()
	g.calls = append(g.calls, gicCall{intid, level})
	g.mu.Unlock()
	return nil
}

// newTestHVVM creates a minimal hvVM for handler testing (no HV framework).
func newTestHVVM() (*hvVM, *gicRecorder) {
	gic := &gicRecorder{}
	vm := &hvVM{
		wakeCh:    make(chan struct{}, 1),
		stateCh:   make(chan VMState, 4),
		gicSetSPI: gic.set,
	}
	return vm, gic
}

// --- advancePC ---

func TestAdvancePC(t *testing.T) {
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegPC] = 0x1000
	advancePC(vcpu)
	assert.Equal(t, uint64(0x1004), vcpu.regs[hv.RegPC])
}

// --- PSCI ---

func TestHandlePSCI_Version(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegPC] = 0x1000
	vcpu.regs[hv.RegX0] = psciVersion

	cont := vm.handlePSCI(vcpu, 0)

	assert.True(t, cont, "PSCI_VERSION should not stop the vCPU")
	assert.Equal(t, uint64(0x0001_0000), vcpu.regs[hv.RegX0], "should return PSCI v1.0")
	assert.Equal(t, uint64(0x1004), vcpu.regs[hv.RegPC], "should advance PC")
}

func TestHandlePSCI_SystemOff(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegX0] = psciSystemOff

	cont := vm.handlePSCI(vcpu, 0)

	assert.False(t, cont, "SYSTEM_OFF should stop the vCPU")
	assert.True(t, vm.stopped)
}

func TestHandlePSCI_SystemReset(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegX0] = psciSystemRst

	cont := vm.handlePSCI(vcpu, 0)

	assert.False(t, cont, "SYSTEM_RESET should stop the vCPU")
	assert.True(t, vm.stopped)
}

func TestHandlePSCI_CPUOn_NotSupported(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegPC] = 0x2000
	vcpu.regs[hv.RegX0] = psciCPUOn64

	cont := vm.handlePSCI(vcpu, 0)

	assert.True(t, cont)
	assert.Equal(t, psciNotSupported, vcpu.regs[hv.RegX0])
	assert.Equal(t, uint64(0x2004), vcpu.regs[hv.RegPC])
}

func TestHandlePSCI_UnknownFunction(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegPC] = 0x3000
	vcpu.regs[hv.RegX0] = 0xDEAD_BEEF

	cont := vm.handlePSCI(vcpu, 0)

	assert.True(t, cont)
	assert.Equal(t, psciNotSupported, vcpu.regs[hv.RegX0])
	assert.Equal(t, uint64(0x3004), vcpu.regs[hv.RegPC])
}

// --- syncVTimer ---

func TestSyncVTimer_Asserting(t *testing.T) {
	vcpu := newFakeVCPU()
	// Timer: enabled (bit 0), NOT masked (bit 1 clear), status set (bit 2).
	vcpu.sysRegs[hv.SysRegCNTV_CTL_EL0] = (1 << 0) | (1 << 2) // enable + istatus
	masked := syncVTimer(vcpu)
	// Timer still asserting — guest hasn't handled it yet. Leave masked
	// to avoid a tight ExitVTimer loop that starves the guest CPU.
	assert.True(t, masked, "should stay masked when asserting")
	assert.Empty(t, vcpu.vtimerMaskCalls, "should NOT call SetVTimerMask")
}

func TestSyncVTimer_NotAsserting_Disabled(t *testing.T) {
	vcpu := newFakeVCPU()
	// Timer: disabled.
	vcpu.sysRegs[hv.SysRegCNTV_CTL_EL0] = 0
	masked := syncVTimer(vcpu)
	assert.False(t, masked, "timer not asserting, should unmask")
	require.Len(t, vcpu.vtimerMaskCalls, 1)
	assert.False(t, vcpu.vtimerMaskCalls[0], "should call SetVTimerMask(false)")
}

func TestSyncVTimer_NotAsserting_Masked(t *testing.T) {
	vcpu := newFakeVCPU()
	// Timer: enabled, interrupt masked (bit 1), status set.
	vcpu.sysRegs[hv.SysRegCNTV_CTL_EL0] = (1 << 0) | (1 << 1) | (1 << 2)
	masked := syncVTimer(vcpu)
	assert.False(t, masked, "timer masked by guest, should unmask HV mask")
	require.Len(t, vcpu.vtimerMaskCalls, 1)
	assert.False(t, vcpu.vtimerMaskCalls[0])
}

// --- drainPendingIRQs ---

func TestDrainPendingIRQs_Empty(t *testing.T) {
	vm, gic := newTestHVVM()
	vcpu := newFakeVCPU()
	vm.drainPendingIRQs(vcpu)
	assert.Empty(t, gic.calls)
}

func TestDrainPendingIRQs_SingleIRQ(t *testing.T) {
	vm, gic := newTestHVVM()
	vcpu := newFakeVCPU()
	// SPI 48 = intid 48, offset from spiBase(32) = 16
	vm.pendingSPIs.Or(1 << (48 - spiBase))

	vm.drainPendingIRQs(vcpu)

	require.Len(t, gic.calls, 2)
	assert.Equal(t, gicCall{48, false}, gic.calls[0], "deassert first")
	assert.Equal(t, gicCall{48, true}, gic.calls[1], "then assert")
}

func TestDrainPendingIRQs_MultipleIRQs(t *testing.T) {
	vm, gic := newTestHVVM()
	vcpu := newFakeVCPU()
	vm.pendingSPIs.Or(1 << (48 - spiBase))
	vm.pendingSPIs.Or(1 << (49 - spiBase))
	vm.pendingSPIs.Or(1 << (50 - spiBase))

	vm.drainPendingIRQs(vcpu)

	assert.Len(t, gic.calls, 6, "3 IRQs × 2 calls each")
}

func TestDrainPendingIRQs_Dedup(t *testing.T) {
	vm, gic := newTestHVVM()
	vcpu := newFakeVCPU()
	// Setting the same bit multiple times is a no-op — only one edge pulse.
	vm.pendingSPIs.Or(1 << (50 - spiBase))
	vm.pendingSPIs.Or(1 << (50 - spiBase))
	vm.pendingSPIs.Or(1 << (50 - spiBase))

	vm.drainPendingIRQs(vcpu)

	require.Len(t, gic.calls, 2, "deduped to one edge pulse")
	assert.Equal(t, gicCall{50, false}, gic.calls[0])
	assert.Equal(t, gicCall{50, true}, gic.calls[1])
}

// --- handleMMIO ---

func TestHandleMMIO_Write(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegPC] = 0x1000

	// Register a test MMIO device.
	dev := &mmioRecorder{}
	vm.bus.Register(0x0900_0000, 0x1000, dev)

	// ISS for a 32-bit write: ISV=1, SAS=2 (32-bit), SRT=3, WnR=1.
	iss := uint64(1)<<24 | uint64(2)<<22 | uint64(3)<<16 | uint64(1)<<6
	exit := hv.ExitInfo{
		Syndrome:        uint64(hv.ECDataAbort)<<26 | iss,
		PhysicalAddress: 0x0900_0000,
	}
	vcpu.regs[hv.RegX3] = 0xCAFE

	vm.handleMMIO(vcpu, 0, exit)

	require.Len(t, dev.writes, 1)
	assert.Equal(t, uint64(0), dev.writes[0].offset)
	assert.Equal(t, uint32(4), dev.writes[0].size)
	assert.Equal(t, uint64(0xCAFE), dev.writes[0].val)
	assert.Equal(t, uint64(0x1004), vcpu.regs[hv.RegPC])
}

func TestHandleMMIO_Read(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegPC] = 0x2000

	dev := &mmioRecorder{readVal: 0x42}
	vm.bus.Register(0x0A00_0000, 0x200, dev)

	// ISS for a 32-bit read: ISV=1, SAS=2 (32-bit), SRT=5, WnR=0.
	iss := uint64(1)<<24 | uint64(2)<<22 | uint64(5)<<16
	exit := hv.ExitInfo{
		Syndrome:        uint64(hv.ECDataAbort)<<26 | iss,
		PhysicalAddress: 0x0A00_0010,
	}

	vm.handleMMIO(vcpu, 0, exit)

	assert.Equal(t, uint64(0x42), vcpu.regs[hv.RegX5])
	assert.Equal(t, uint64(0x2004), vcpu.regs[hv.RegPC])
}

// --- handleException dispatch ---

func TestHandleException_WFI(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegPC] = 0x1000

	// WFI: EC=0x01, syndrome bit 0 = 0 (WFI not WFE).
	exit := hv.ExitInfo{Syndrome: uint64(hv.ECWFx) << 26}

	// Pre-signal wakeCh so WFI doesn't block.
	vm.wakeCh <- struct{}{}

	cont := vm.handleException(vcpu, 0, exit)
	assert.True(t, cont)
	assert.Equal(t, uint64(0x1004), vcpu.regs[hv.RegPC])
}

func TestHandleException_SMC(t *testing.T) {
	vm, _ := newTestHVVM()
	vcpu := newFakeVCPU()
	vcpu.regs[hv.RegPC] = 0x1000
	vcpu.regs[hv.RegX0] = psciVersion

	exit := hv.ExitInfo{Syndrome: uint64(hv.ECSMC64) << 26}
	cont := vm.handleException(vcpu, 0, exit)

	assert.True(t, cont)
	assert.Equal(t, uint64(0x0001_0000), vcpu.regs[hv.RegX0])
}

// mmioRecorder is a test MMIO device that records accesses.
type mmioRecorder struct {
	reads   []mmioAccess
	writes  []mmioAccess
	readVal uint64
}

type mmioAccess struct {
	offset uint64
	size   uint32
	val    uint64
}

func (m *mmioRecorder) Read(offset uint64, size uint32) uint64 {
	m.reads = append(m.reads, mmioAccess{offset: offset, size: size})
	return m.readVal
}

func (m *mmioRecorder) Write(offset uint64, size uint32, val uint64) {
	m.writes = append(m.writes, mmioAccess{offset: offset, size: size, val: val})
}
