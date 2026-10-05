//go:build darwin

package hv

import (
	"encoding/binary"
	"fmt"
)

// FDT builder — generates a Flattened Device Tree blob (DTB).
// Implements the minimal subset needed for the ARM virt machine.
//
// Reference: https://devicetree-specification.readthedocs.io/en/latest/flattened-format.html

const (
	fdtMagic      = 0xd00dfeed
	fdtBeginNode  = 0x00000001
	fdtEndNode    = 0x00000002
	fdtProp       = 0x00000003
	fdtEnd        = 0x00000009
	fdtHeaderSize = 40
)

// FDT builds a Flattened Device Tree blob.
type FDT struct {
	structure []byte
	strings   []byte
	strIndex  map[string]uint32
}

// NewFDT creates a new FDT builder.
func NewFDT() *FDT {
	return &FDT{strIndex: make(map[string]uint32)}
}

// BeginNode starts a new node with the given name.
func (f *FDT) BeginNode(name string) {
	f.putU32(fdtBeginNode)
	f.putString(name)
}

// EndNode closes the current node.
func (f *FDT) EndNode() {
	f.putU32(fdtEndNode)
}

// PropU32 adds a 32-bit integer property.
func (f *FDT) PropU32(name string, val uint32) {
	f.putU32(fdtProp)
	f.putU32(4)
	f.putU32(f.strOff(name))
	f.putU32(val)
}

// PropU64 adds a 64-bit integer property.
func (f *FDT) PropU64(name string, val uint64) {
	f.putU32(fdtProp)
	f.putU32(8)
	f.putU32(f.strOff(name))
	f.putU32(uint32(val >> 32))
	f.putU32(uint32(val))
}

// PropStr adds a null-terminated string property.
func (f *FDT) PropStr(name, val string) {
	data := append([]byte(val), 0)
	f.prop(name, data)
}

// PropStrList adds a property with multiple null-terminated strings.
func (f *FDT) PropStrList(name string, vals ...string) {
	var data []byte
	for _, v := range vals {
		data = append(data, v...)
		data = append(data, 0)
	}
	f.prop(name, data)
}

// PropEmpty adds a property with no value (boolean marker).
func (f *FDT) PropEmpty(name string) {
	f.prop(name, nil)
}

// PropCells adds a property consisting of 32-bit cells.
func (f *FDT) PropCells(name string, cells ...uint32) {
	data := make([]byte, len(cells)*4)
	for i, c := range cells {
		binary.BigEndian.PutUint32(data[i*4:], c)
	}
	f.prop(name, data)
}

// PropReg adds a "reg" property with addr/size pairs.
// Each entry is (addrHi, addrLo, sizeHi, sizeLo) for #address-cells=2, #size-cells=2.
func (f *FDT) PropReg(entries ...RegEntry) {
	var cells []uint32
	for _, e := range entries {
		cells = append(cells,
			uint32(e.Addr>>32), uint32(e.Addr),
			uint32(e.Size>>32), uint32(e.Size))
	}
	f.PropCells("reg", cells...)
}

// RegEntry is an address/size pair for a "reg" property.
type RegEntry struct {
	Addr, Size uint64
}

// Finish finalizes the FDT and returns the complete DTB blob.
// Layout: header | mem_rsvmap | structure | strings
func (f *FDT) Finish() []byte {
	f.putU32(fdtEnd)

	// Pad struct block to 4-byte alignment (should already be aligned).
	for len(f.structure)%4 != 0 {
		f.structure = append(f.structure, 0)
	}

	structSize := uint32(len(f.structure))
	stringsSize := uint32(len(f.strings))

	// Memory reservation map: one terminating entry (16 bytes of zeros).
	const rsvmapSize = 16
	offRsvmap := uint32(fdtHeaderSize)
	offStruct := offRsvmap + rsvmapSize
	offStrings := offStruct + structSize
	totalSize := offStrings + stringsSize

	hdr := make([]byte, fdtHeaderSize)
	binary.BigEndian.PutUint32(hdr[0:], fdtMagic)
	binary.BigEndian.PutUint32(hdr[4:], totalSize)
	binary.BigEndian.PutUint32(hdr[8:], offStruct)
	binary.BigEndian.PutUint32(hdr[12:], offStrings)
	binary.BigEndian.PutUint32(hdr[16:], offRsvmap)
	binary.BigEndian.PutUint32(hdr[20:], 17) // version
	binary.BigEndian.PutUint32(hdr[24:], 16) // last_comp_version
	binary.BigEndian.PutUint32(hdr[28:], 0)  // boot_cpuid_phys
	binary.BigEndian.PutUint32(hdr[32:], stringsSize)
	binary.BigEndian.PutUint32(hdr[36:], structSize)

	blob := make([]byte, 0, totalSize)
	blob = append(blob, hdr...)
	blob = append(blob, make([]byte, rsvmapSize)...) // terminating rsvmap entry
	blob = append(blob, f.structure...)
	blob = append(blob, f.strings...)
	return blob
}

// prop adds a raw property.
func (f *FDT) prop(name string, data []byte) {
	f.putU32(fdtProp)
	f.putU32(uint32(len(data)))
	f.putU32(f.strOff(name))
	f.structure = append(f.structure, data...)
	// Pad to 4-byte alignment.
	for len(f.structure)%4 != 0 {
		f.structure = append(f.structure, 0)
	}
}

// strOff returns the string block offset for a property name, adding it if new.
func (f *FDT) strOff(name string) uint32 {
	if off, ok := f.strIndex[name]; ok {
		return off
	}
	off := uint32(len(f.strings))
	f.strIndex[name] = off
	f.strings = append(f.strings, name...)
	f.strings = append(f.strings, 0)
	return off
}

func (f *FDT) putU32(v uint32) {
	var b [4]byte
	binary.BigEndian.PutUint32(b[:], v)
	f.structure = append(f.structure, b[:]...)
}

func (f *FDT) putString(s string) {
	f.structure = append(f.structure, s...)
	f.structure = append(f.structure, 0)
	for len(f.structure)%4 != 0 {
		f.structure = append(f.structure, 0)
	}
}

// BuildVirtDTB generates a device tree blob for the lnx HV virt machine.
// Layout matches QEMU's ARM virt machine.
func BuildVirtDTB(cfg VirtDTBConfig) []byte {
	f := NewFDT()

	const gicPhandle = 1

	// Root node.
	f.BeginNode("")
	f.PropStr("compatible", "linux,dummy-virt")
	f.PropStr("model", "lnx-hv")
	f.PropU32("#address-cells", 2)
	f.PropU32("#size-cells", 2)
	f.PropEmpty("dma-coherent")
	f.PropU32("interrupt-parent", gicPhandle) // default interrupt controller

	// /chosen
	f.BeginNode("chosen")
	f.PropStr("bootargs", cfg.Cmdline)
	if cfg.InitrdStart != 0 {
		f.PropCells("linux,initrd-start",
			uint32(cfg.InitrdStart>>32), uint32(cfg.InitrdStart))
		f.PropCells("linux,initrd-end",
			uint32(cfg.InitrdEnd>>32), uint32(cfg.InitrdEnd))
	}
	f.EndNode()

	// /memory
	f.BeginNode(fmt.Sprintf("memory@%x", cfg.RAMBase))
	f.PropStr("device_type", "memory")
	f.PropReg(RegEntry{cfg.RAMBase, cfg.RAMSize})
	f.EndNode()

	// /cpus
	f.BeginNode("cpus")
	f.PropU32("#address-cells", 1)
	f.PropU32("#size-cells", 0)
	for i := uint(0); i < cfg.CPUs; i++ {
		f.BeginNode(fmt.Sprintf("cpu@%d", i))
		f.PropStr("device_type", "cpu")
		f.PropStr("compatible", "arm,arm-v8")
		f.PropU32("reg", uint32(i))
		f.PropStr("enable-method", "psci")
		f.EndNode()
	}
	f.EndNode()

	// /psci
	f.BeginNode("psci")
	f.PropStr("compatible", "arm,psci-1.0")
	f.PropStr("method", "smc")
	f.EndNode()

	// /timer
	f.BeginNode("timer")
	f.PropStrList("compatible", "arm,armv8-timer", "arm,armv7-timer")
	f.PropEmpty("always-on")
	// PPI interrupts: secure EL1(29), NS EL1(30), virtual(27), NS EL2(26)
	// Format: type, PPI_num (intid-16), flags
	// type: 1 = PPI; flags: 4 = level high (GIC_FDT_IRQ_FLAGS_LEVEL_HI)
	f.PropCells("interrupts",
		1, 29-16, 4, // secure EL1 physical timer
		1, 30-16, 4, // NS EL1 physical timer
		1, 27-16, 4, // virtual timer
		1, 26-16, 4, // NS EL2 physical timer
	)
	f.EndNode()

	// /intc (GICv3) — must be before nodes that reference it.
	f.BeginNode(fmt.Sprintf("intc@%x", cfg.GICDistBase))
	f.PropStr("compatible", "arm,gic-v3")
	f.PropU32("#interrupt-cells", 3)
	f.PropEmpty("interrupt-controller")
	f.PropU32("#address-cells", 2)
	f.PropU32("#size-cells", 2)
	f.PropEmpty("ranges")
	f.PropU32("#redistributor-regions", 1)
	f.PropU32("phandle", gicPhandle)
	f.PropReg(
		RegEntry{cfg.GICDistBase, cfg.GICDistSize},
		RegEntry{cfg.GICRedistBase, cfg.GICRedistSize},
	)
	f.EndNode()

	// /apb-pclk (fixed clock for PL011)
	const clkPhandle = 2
	if cfg.UARTBase != 0 {
		f.BeginNode("apb-pclk")
		f.PropStr("compatible", "fixed-clock")
		f.PropU32("#clock-cells", 0)
		f.PropU32("clock-frequency", 24000000)
		f.PropStr("clock-output-names", "clk24mhz")
		f.PropU32("phandle", clkPhandle)
		f.EndNode()

		// /uart (PL011)
		f.BeginNode(fmt.Sprintf("pl011@%x", cfg.UARTBase))
		f.PropStrList("compatible", "arm,pl011", "arm,primecell")
		f.PropReg(RegEntry{cfg.UARTBase, 0x1000})
		f.PropCells("interrupts",
			0,           // SPI
			cfg.UARTIRQ, // IRQ number
			4,           // level high
		)
		f.PropStrList("clock-names", "uartclk", "apb_pclk")
		f.PropCells("clocks", clkPhandle, clkPhandle)
		f.EndNode()
	}

	// /virtio_mmio devices
	for i := int(cfg.VirtioCount) - 1; i >= 0; i-- {
		base := cfg.VirtioBase + uint64(i)*cfg.VirtioStride
		irq := cfg.VirtioIRQBase + uint32(i)
		f.BeginNode(fmt.Sprintf("virtio_mmio@%x", base))
		f.PropStr("compatible", "virtio,mmio")
		f.PropReg(RegEntry{base, cfg.VirtioStride})
		f.PropCells("interrupts",
			0, // SPI
			irq,
			4, // level high — required for HV.framework's GIC
		)
		f.PropEmpty("dma-coherent")
		f.EndNode()
	}

	f.EndNode() // root
	return f.Finish()
}

// VirtDTBConfig holds parameters for DTB generation.
type VirtDTBConfig struct {
	RAMBase  uint64
	RAMSize  uint64
	CPUs     uint
	Cmdline  string

	GICDistBase   uint64
	GICDistSize   uint64
	GICRedistBase uint64
	GICRedistSize uint64

	InitrdStart uint64
	InitrdEnd   uint64

	UARTBase uint64
	UARTIRQ  uint32

	VirtioBase    uint64
	VirtioStride  uint64
	VirtioCount   uint
	VirtioIRQBase uint32 // first SPI number
}
