//go:build darwin

package hv

// #cgo LDFLAGS: -framework Hypervisor
// #include <Hypervisor/Hypervisor.h>
import "C"

import (
	"fmt"
	"log/slog"
)

// GICConfig wraps an hv_gic_config_t for configuring the in-framework GICv3.
type GICConfig struct {
	cfg C.hv_gic_config_t
}

// NewGICConfig creates a GIC configuration object.
// Must be called after VMCreate.
func NewGICConfig() *GICConfig {
	return &GICConfig{cfg: C.hv_gic_config_create()}
}

// SetDistributorBase sets the GIC distributor base guest physical address.
func (g *GICConfig) SetDistributorBase(addr uint64) error {
	return hvErr(C.hv_gic_config_set_distributor_base(g.cfg, C.hv_ipa_t(addr)))
}

// SetRedistributorBase sets the GIC redistributor region base guest physical address.
func (g *GICConfig) SetRedistributorBase(addr uint64) error {
	return hvErr(C.hv_gic_config_set_redistributor_base(g.cfg, C.hv_ipa_t(addr)))
}

// GICCreate creates a GICv3 from the configuration.
// Must be called after VMCreate and before any vCPU creation.
func GICCreate(cfg *GICConfig) error {
	return hvErr(C.hv_gic_create(cfg.cfg))
}

// GICSetSPI asserts or deasserts a Shared Peripheral Interrupt.
func GICSetSPI(intid uint32, level bool) error {
	return hvErr(C.hv_gic_set_spi(C.uint32_t(intid), C.bool(level)))
}

// GICGetRedistributorBase returns the redistributor base address for a vCPU.
// Must be called after the vCPU's MPIDR_EL1 has been set.
func GICGetRedistributorBase(vcpu *VCPU) (uint64, error) {
	var addr C.hv_ipa_t
	if err := hvErr(C.hv_gic_get_redistributor_base(vcpu.id, &addr)); err != nil {
		return 0, err
	}
	return uint64(addr), nil
}

// GICDistRead reads a GIC distributor register at the given offset.
func GICDistRead(offset uint64) (uint64, error) {
	var val C.uint64_t
	if err := hvErr(C.hv_gic_get_distributor_reg(C.hv_gic_distributor_reg_t(offset), &val)); err != nil {
		return 0, err
	}
	return uint64(val), nil
}

// GICDistWrite writes a GIC distributor register at the given offset.
func GICDistWrite(offset uint64, val uint64) error {
	return hvErr(C.hv_gic_set_distributor_reg(C.hv_gic_distributor_reg_t(offset), C.uint64_t(val)))
}

// GICRedistRead reads a GIC redistributor register for a vCPU.
func GICRedistRead(vcpu *VCPU, offset uint64) (uint64, error) {
	var val C.uint64_t
	if err := hvErr(C.hv_gic_get_redistributor_reg(vcpu.id, C.hv_gic_redistributor_reg_t(offset), &val)); err != nil {
		return 0, err
	}
	return uint64(val), nil
}

// GICRedistWrite writes a GIC redistributor register for a vCPU.
func GICRedistWrite(vcpu *VCPU, offset uint64, val uint64) error {
	return hvErr(C.hv_gic_set_redistributor_reg(vcpu.id, C.hv_gic_redistributor_reg_t(offset), C.uint64_t(val)))
}

// GICMMIO is an MMIODevice that forwards accesses to the HV.framework GIC.
type GICMMIO struct {
	vcpu *VCPU // for redistributor (per-CPU)
	dist bool  // true for distributor, false for redistributor
}

// NewGICDistMMIO creates an MMIO forwarder for the GIC distributor.
func NewGICDistMMIO() *GICMMIO {
	return &GICMMIO{dist: true}
}

// NewGICRedistMMIO creates an MMIO forwarder for a vCPU's redistributor.
func NewGICRedistMMIO(vcpu *VCPU) *GICMMIO {
	return &GICMMIO{vcpu: vcpu, dist: false}
}

func (g *GICMMIO) Read(offset uint64, size uint32) uint64 {
	var val uint64
	var err error
	if g.dist {
		val, err = GICDistRead(offset)
	} else {
		val, err = GICRedistRead(g.vcpu, offset)
	}
	if err != nil {
		return 0
	}
	return val
}

func (g *GICMMIO) Write(offset uint64, size uint32, val uint64) {
	if g.dist {
		if err := GICDistWrite(offset, val); err != nil {
			slog.Error("GIC dist write failed", "offset", fmt.Sprintf("0x%x", offset), "val", fmt.Sprintf("0x%x", val), "error", err)
		}
	} else {
		GICRedistWrite(g.vcpu, offset, val)
	}
}
