// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io;
use std::result;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use super::super::{FC_EXIT_CODE_GENERIC_ERROR, FC_EXIT_CODE_OK};
use crate::vmm_config::machine_config::CpuFeaturesTemplate;

use arch::ArchMemoryInfo;
use crossbeam_channel::{Receiver, RecvTimeoutError, Select, Sender, TryRecvError, unbounded};
use devices::legacy::VcpuList;
use hvf::{HvfVcpu, HvfVm, InterruptController, VcpuExit, Vcpus};
use serde::Deserialize;
use utils::eventfd::EventFd;
use vm_memory::{
    Address, GuestAddress, GuestMemory, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion,
};

static VCPU_EXIT_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);

/// Errors associated with the wrappers over KVM ioctls.
#[derive(Debug)]
pub enum Error {
    /// Invalid guest memory configuration.
    GuestMemoryMmap(GuestMemoryError),
    /// The number of configured slots is bigger than the maximum reported by KVM.
    NotEnoughMemorySlots,
    /// Error configuring the general purpose aarch64 registers.
    REGSConfiguration(arch::aarch64::regs::Error),
    /// Cannot set the memory regions.
    SetUserMemoryRegion(hvf::Error),
    /// Failed to signal Vcpu.
    SignalVcpu(utils::errno::Error),
    /// Error doing Vcpu Init on Arm.
    VcpuArmInit,
    /// Error getting the Vcpu preferred target on Arm.
    VcpuArmPreferredTarget,
    /// vCPU count is not initialized.
    VcpuCountNotInitialized,
    /// Cannot run the VCPUs.
    VcpuRun,
    /// Cannot spawn a new vCPU thread.
    VcpuSpawn(io::Error),
    /// The vCPU thread has exited and no longer accepts events.
    VcpuThreadGone,
    /// Cannot cleanly initialize vcpu TLS.
    VcpuTlsInit,
    /// Vcpu not present in TLS.
    VcpuTlsNotPresent,
    /// Unexpected KVM_RUN exit reason
    VcpuUnhandledKvmExit,
    /// Cannot configure the microvm.
    VmSetup(hvf::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            GuestMemoryMmap(e) => write!(f, "Guest memory error: {e:?}"),
            VcpuCountNotInitialized => write!(f, "vCPU count is not initialized"),
            VmSetup(e) => write!(f, "Cannot configure the microvm: {e:?}"),
            VcpuRun => write!(f, "Cannot run the VCPUs"),
            NotEnoughMemorySlots => write!(
                f,
                "The number of configured slots is bigger than the maximum reported by KVM"
            ),
            SetUserMemoryRegion(e) => write!(f, "Cannot set the memory regions: {e:?}"),
            SignalVcpu(e) => write!(f, "Failed to signal Vcpu: {e}"),
            REGSConfiguration(e) => write!(
                f,
                "Error configuring the general purpose aarch64 registers: {e:?}"
            ),
            VcpuSpawn(e) => write!(f, "Cannot spawn a new vCPU thread: {e}"),
            VcpuThreadGone => write!(f, "vCPU thread has exited"),
            VcpuTlsInit => write!(f, "Cannot clean init vcpu TLS"),
            VcpuTlsNotPresent => write!(f, "Vcpu not present in TLS"),
            VcpuUnhandledKvmExit => write!(f, "Unexpected KVM_RUN exit reason"),
            VcpuArmPreferredTarget => write!(f, "Error getting the Vcpu preferred target on Arm"),
            VcpuArmInit => write!(f, "Error doing Vcpu Init on Arm"),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

/// A wrapper around creating and using a VM.
pub struct Vm {
    hvf_vm: HvfVm,
}

impl Vm {
    /// Constructs a new `Vm` using the given `Kvm` instance.
    pub fn new(nested_enabled: bool) -> Result<Self> {
        let hvf_vm = HvfVm::new(nested_enabled).map_err(Error::VmSetup)?;

        Ok(Vm { hvf_vm })
    }

    pub fn hvf_vm(&self) -> &HvfVm {
        &self.hvf_vm
    }

    /// Initializes the guest memory.
    pub fn memory_init(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        excluded_ranges: &[(GuestAddress, usize)],
    ) -> Result<()> {
        for region in guest_mem.iter() {
            if excluded_ranges
                .iter()
                .any(|(addr, size)| *addr == region.start_addr() && *size as u64 == region.len())
            {
                continue;
            }
            // It's safe to unwrap because the guest address is valid.
            let host_addr = guest_mem.get_host_address(region.start_addr()).unwrap();
            debug!(
                "Guest memory host_addr={:x?} guest_addr={:x?} len={:x?}",
                host_addr,
                region.start_addr().raw_value(),
                region.len()
            );
            self.hvf_vm
                .map_memory(
                    host_addr as u64,
                    region.start_addr().raw_value(),
                    region.len(),
                )
                .map_err(Error::SetUserMemoryRegion)?;
        }

        Ok(())
    }

    pub fn add_mapping(
        &self,
        reply_sender: Sender<bool>,
        host_addr: u64,
        guest_addr: u64,
        len: u64,
        protection: u64,
    ) {
        debug!(
            "add_mapping: host_addr={host_addr:x}, guest_addr={guest_addr:x}, len={len}, protection={protection}"
        );
        if let Err(e) = self.hvf_vm.unmap_memory(guest_addr, len) {
            error!("Error removing memory map: {e:?}");
        }

        if let Err(e) = self
            .hvf_vm
            .map_memory_with_protection(host_addr, guest_addr, len, protection)
        {
            error!("Error adding memory map: {e:?}");
            reply_sender.send(false).unwrap();
        } else {
            reply_sender.send(true).unwrap();
        }
    }

    pub fn remove_mapping(&self, reply_sender: Sender<bool>, guest_addr: u64, len: u64) {
        debug!("remove_mapping: guest_addr={guest_addr:x}, len={len}");
        if let Err(e) = self.hvf_vm.unmap_memory(guest_addr, len) {
            error!("Error removing memory map: {e:?}");
            reply_sender.send(false).unwrap();
        } else {
            reply_sender.send(true).unwrap();
        }
    }
}

/// Encapsulates configuration parameters for the guest vCPUS.
#[derive(Debug, Eq, PartialEq)]
pub struct VcpuConfig {
    /// Number of guest VCPUs.
    pub vcpu_count: u8,
    /// Enable hyperthreading in the CPUID configuration.
    pub ht_enabled: bool,
    /// CPUID template to use.
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

// Using this for easier explicit type-casting to help IDEs interpret the code.
type VcpuCell = Cell<Option<*const Vcpu>>;

/// A wrapper around creating and using a kvm-based VCPU.
pub struct Vcpu {
    id: u8,
    boot_entry_addr: u64,
    boot_receiver: Option<Receiver<u64>>,
    boot_senders: Option<HashMap<u64, Sender<u64>>>,
    fdt_addr: u64,
    mmio_bus: Option<devices::Bus>,
    #[cfg_attr(all(test, target_arch = "aarch64"), allow(unused))]
    exit_evt: EventFd,

    #[cfg(target_arch = "aarch64")]
    mpidr: u64,

    event_receiver: Receiver<VcpuRequest>,
    // The transmitting end of the events channel which will be given to the handler.
    event_sender: Option<Sender<VcpuRequest>>,
    // The receiving end of the responses channel which will be given to the handler.
    response_receiver: Option<Receiver<VcpuReport>>,
    // The transmitting end of the responses channel owned by the vcpu side.
    response_sender: Sender<VcpuReport>,

    vcpu_list: Arc<VcpuList>,
    nested_enabled: bool,
    interrupt_controller: InterruptController,
    initial_pause: bool,
}

impl Vcpu {
    thread_local!(static TLS_VCPU_PTR: VcpuCell = const { Cell::new(None) });

    /// Associates `self` with the current thread.
    ///
    /// It is a prerequisite to successfully run `init_thread_local_data()` before using
    /// `run_on_thread_local()` on the current thread.
    /// This function will return an error if there already is a `Vcpu` present in the TLS.
    fn init_thread_local_data(&mut self) -> Result<()> {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if cell.get().is_some() {
                return Err(Error::VcpuTlsInit);
            }
            cell.set(Some(self as *const Vcpu));
            Ok(())
        })
    }

    /// Deassociates `self` from the current thread.
    ///
    /// Should be called if the current `self` had called `init_thread_local_data()` and
    /// now needs to move to a different thread.
    ///
    /// Fails if `self` was not previously associated with the current thread.
    fn reset_thread_local_data(&mut self) -> Result<()> {
        // Best-effort to clean up TLS. If the `Vcpu` was moved to another thread
        // _before_ running this, then there is nothing we can do.
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if let Some(vcpu_ptr) = cell.get()
                && std::ptr::eq(vcpu_ptr, self)
            {
                Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| cell.take());
                return Ok(());
            }
            Err(Error::VcpuTlsNotPresent)
        })
    }

    /// Registers a signal handler which makes use of TLS and kvm immediate exit to
    /// kick the vcpu running on the current thread, if there is one.
    pub fn register_kick_signal_handler() {
        /*
        extern "C" fn handle_signal(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
            // This is safe because it's temporarily aliasing the `Vcpu` object, but we are
            // only reading `vcpu.fd` which does not change for the lifetime of the `Vcpu`.
            unsafe {
                let _ = Vcpu::run_on_thread_local(|_vcpu| {
                    vcpu.fd.set_kvm_immediate_exit(1);
                    fence(Ordering::Release);
                });
            }
        }
        */

        //register_signal_handler(sigrtmin() + VCPU_RTSIG_OFFSET, handle_signal)
        //    .expect("Failed to register vcpu signal handler");
    }

    /// Constructs a new VCPU for `vm`.
    ///
    /// # Arguments
    ///
    /// * `id` - Represents the CPU number between [0, max vcpus).
    /// * `vm_fd` - The kvm `VmFd` for the virtual machine this vcpu will get attached to.
    /// * `exit_evt` - An `EventFd` that will be written into when this vcpu exits.
    /// * `interrupt_controller` - The GIC the VM was built with.
    pub fn new_aarch64(
        id: u8,
        boot_entry_addr: GuestAddress,
        boot_receiver: Option<Receiver<u64>>,
        exit_evt: EventFd,
        vcpu_list: Arc<VcpuList>,
        nested_enabled: bool,
        interrupt_controller: InterruptController,
    ) -> Result<Self> {
        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        Ok(Vcpu {
            id,
            boot_entry_addr: boot_entry_addr.raw_value(),
            boot_receiver,
            boot_senders: None,
            fdt_addr: 0,
            mmio_bus: None,
            exit_evt,
            mpidr: id as u64,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
            vcpu_list,
            nested_enabled,
            interrupt_controller,
            initial_pause: false,
        })
    }

    /// Returns the cpu index as seen by the guest OS.
    pub fn cpu_index(&self) -> u8 {
        self.id
    }

    /// Gets the MPIDR register value.
    pub fn get_mpidr(&self) -> u64 {
        self.mpidr
    }

    /// Sets a MMIO bus for this vcpu.
    pub fn set_mmio_bus(&mut self, mmio_bus: devices::Bus) {
        self.mmio_bus = Some(mmio_bus);
    }

    pub fn set_boot_senders(&mut self, boot_senders: HashMap<u64, Sender<u64>>) {
        self.boot_senders = Some(boot_senders);
    }

    /// Pre-queue a Pause event so the vCPU thread immediately blocks at the
    /// top of its first loop iteration, *before* running any guest code.
    /// Used during snapshot restore to prevent a brief boot-code execution
    /// window before restore state is applied.
    pub fn queue_initial_pause(&mut self) {
        self.initial_pause = true;
        if let Some(sender) = &self.event_sender {
            let _ = sender.send(VcpuRequest {
                seq: INITIAL_PAUSE_SEQ,
                event: VcpuEvent::Pause,
            });
        }
    }

    /// Configures an aarch64 specific vcpu.
    ///
    /// # Arguments
    ///
    /// * `guest_mem` - The guest memory used by this microvm.
    pub fn configure_aarch64(&mut self, mem_info: &ArchMemoryInfo) -> Result<()> {
        self.fdt_addr = mem_info.fdt_addr;

        Ok(())
    }

    /// Moves the vcpu to its own thread and constructs a VcpuHandle.
    /// The handle can be used to control the remote vcpu.
    pub fn start_threaded(mut self) -> Result<VcpuHandle> {
        let event_sender = self.event_sender.take().unwrap();
        let response_receiver = self.response_receiver.take().unwrap();
        let (init_tls_sender, init_tls_receiver) = unbounded();

        let vcpu_thread = thread::Builder::new()
            .name(format!("fc_vcpu {}", self.cpu_index()))
            .spawn(move || {
                self.init_thread_local_data()
                    .expect("Cannot cleanly initialize vcpu TLS.");

                self.run(init_tls_sender);
            })
            .map_err(Error::VcpuSpawn)?;

        init_tls_receiver
            .recv()
            .expect("Error waiting for TLS initialization.");

        Ok(VcpuHandle::new(
            event_sender,
            response_receiver,
            vcpu_thread,
        ))
    }

    /// Returns error or enum specifying whether emulation was handled or interrupted.
    fn run_emulation(&mut self, hvf_vcpu: &mut HvfVcpu) -> Result<VcpuEmulation> {
        let vcpuid = hvf_vcpu.id();

        match hvf_vcpu.run(self.vcpu_list.clone()) {
            Ok(exit) => match exit {
                VcpuExit::Breakpoint => {
                    debug_log_vcpu_exit(vcpuid, "breakpoint".to_string());
                    Ok(VcpuEmulation::Interrupted)
                }
                VcpuExit::Canceled => {
                    debug_log_vcpu_exit(vcpuid, "canceled".to_string());
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::CpuOn(mpidr, entry, context_id) => {
                    debug!("CpuOn: mpidr=0x{mpidr:x} entry=0x{entry:x} context_id={context_id}");
                    if let Some(boot_senders) = &self.boot_senders {
                        if let Some(sender) = boot_senders.get(&mpidr) {
                            sender.send(entry).unwrap()
                        }
                    } else {
                        error!("CpuOn request coming from an unexpected vCPU={}", self.id);
                    }
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::HypervisorCall => {
                    debug_log_vcpu_exit(vcpuid, "hvc".to_string());
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::MmioRead(addr, data) => {
                    debug_log_vcpu_exit(
                        vcpuid,
                        format!("mmio_read addr=0x{addr:x} len={}", data.len()),
                    );
                    if let Some(ref mmio_bus) = self.mmio_bus {
                        debug!("vCPU {vcpuid} MMIO read 0x{addr:x}");
                        mmio_bus.read(vcpuid, addr, data);
                    }
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::MmioWrite(addr, data) => {
                    debug_log_vcpu_exit(
                        vcpuid,
                        format!(
                            "mmio_write addr=0x{addr:x} len={} data={data:x?}",
                            data.len()
                        ),
                    );
                    if let Some(ref mmio_bus) = self.mmio_bus {
                        mmio_bus.write(vcpuid, addr, data);
                    }
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::DirtyMemory => {
                    debug_log_vcpu_exit(vcpuid, "dirty_memory".to_string());
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::PsciHandled => {
                    debug_log_vcpu_exit(vcpuid, "psci".to_string());
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::SecureMonitorCall => {
                    debug_log_vcpu_exit(vcpuid, "smc".to_string());
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::Shutdown => {
                    info!("vCPU {vcpuid} received shutdown signal");
                    Ok(VcpuEmulation::Stopped)
                }
                VcpuExit::SystemRegister => {
                    debug_log_vcpu_exit(vcpuid, "system_register".to_string());
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::VtimerActivated => {
                    debug_log_vcpu_exit(vcpuid, "vtimer_activated".to_string());
                    self.vcpu_list.set_vtimer_irq(vcpuid);
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::WaitForEvent => {
                    debug!("vCPU {vcpuid} WaitForEvent");
                    Ok(VcpuEmulation::WaitForEvent)
                }
                VcpuExit::WaitForEventCompleted => {
                    debug_log_vcpu_exit(vcpuid, "wait_for_event_completed".to_string());
                    Ok(VcpuEmulation::Handled)
                }
                VcpuExit::WaitForEventExpired => {
                    debug!("vCPU {vcpuid} WaitForEventExpired");
                    Ok(VcpuEmulation::WaitForEventExpired)
                }
                VcpuExit::WaitForEventTimeout(duration) => {
                    debug!("vCPU {vcpuid} WaitForEventTimeout timeout={duration:?}");
                    Ok(VcpuEmulation::WaitForEventTimeout(duration))
                }
            },
            Err(e) => {
                error!("vCPU {vcpuid} stopped: {e}");
                crate::timing_event(&format!("vcpu.run.error vcpu={vcpuid} error={e}"));
                Err(Error::VcpuRun)
            }
        }
    }

    /// Main loop of the vCPU thread.
    pub fn run(&mut self, init_tls_sender: Sender<bool>) {
        let hvf_vcpu = HvfVcpu::new(self.mpidr, self.nested_enabled, self.interrupt_controller);

        init_tls_sender
            .send(true)
            .expect("Cannot notify vcpu TLS initialization.");

        let mut hvf_vcpu = match hvf_vcpu {
            Ok(hvf_vcpu) => hvf_vcpu,
            Err(e) => {
                error!("vCPU {} could not be created: {e}", self.id);
                self.exit(FC_EXIT_CODE_GENERIC_ERROR);
                return;
            }
        };
        let hvf_vcpuid = hvf_vcpu.id();

        let wfe_receiver = self.vcpu_list.register(hvf_vcpuid);

        let entry_addr = if self.initial_pause {
            self.boot_entry_addr
        } else if let Some(boot_receiver) = &self.boot_receiver {
            boot_receiver.recv().unwrap()
        } else {
            self.boot_entry_addr
        };

        hvf_vcpu
            .set_initial_state(entry_addr, self.fdt_addr)
            .unwrap_or_else(|_| panic!("Can't set HVF vCPU {hvf_vcpuid} initial state"));

        loop {
            if let Err(e) = hvf_vcpu.complete_pending_emulation() {
                error!("vCPU {hvf_vcpuid} pending emulation completion failed: {e:?}");
                self.exit(FC_EXIT_CODE_GENERIC_ERROR);
                break;
            }
            // Service any pending control events (Pause, RestoreState, RebaseTimer)
            // before each HVF run step. process_control_events blocks while paused.
            if !self.process_control_events(&mut hvf_vcpu) {
                self.exit(FC_EXIT_CODE_OK);
                break;
            }
            match self.run_emulation(&mut hvf_vcpu) {
                // Emulation ran successfully, continue.
                Ok(VcpuEmulation::Handled) => (),
                // Emulation was interrupted by a breakpoint.
                Ok(VcpuEmulation::Interrupted) => self.wait_for_resume(),
                // Wait for an external event.
                Ok(VcpuEmulation::WaitForEvent) => {
                    self.wait_for_event(hvf_vcpuid, &wfe_receiver, None)
                }
                Ok(VcpuEmulation::WaitForEventExpired) => {
                    self.vcpu_list.set_vtimer_irq(hvf_vcpuid);
                }
                Ok(VcpuEmulation::WaitForEventTimeout(timeout)) => {
                    self.wait_for_event(hvf_vcpuid, &wfe_receiver, Some(timeout));
                    self.vcpu_list.set_vtimer_irq(hvf_vcpuid);
                }
                // The guest was rebooted or halted.
                Ok(VcpuEmulation::Stopped) => {
                    self.exit(FC_EXIT_CODE_OK);
                    break;
                }
                // Emulation errors lead to vCPU exit.
                Err(_) => {
                    self.exit(FC_EXIT_CODE_GENERIC_ERROR);
                    break;
                }
            }
        }
    }

    /// Drain pending control events. If a Pause was requested, park until
    /// the matching Resume. Returns false if the channel was closed (vmm
    /// dropped), in which case the caller should exit the run loop.
    fn process_control_events(&mut self, hvf_vcpu: &mut HvfVcpu) -> bool {
        loop {
            let VcpuRequest { seq, event } = match self.event_receiver.try_recv() {
                Ok(request) => request,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            };
            match event {
                VcpuEvent::Pause => {
                    self.reply(seq, paused_response(hvf_vcpu));
                    if !self.park(hvf_vcpu) {
                        return false;
                    }
                }
                VcpuEvent::Resume => {
                    // Spurious resume — ack and continue.
                    debug!("vcpu.debug running.resume_ack vcpu={}", hvf_vcpu.id());
                    self.reply(seq, VcpuResponse::Resumed);
                }
                VcpuEvent::RestoreState(_)
                | VcpuEvent::RestoreKvmState { .. }
                | VcpuEvent::RestoreGicRedist(_)
                | VcpuEvent::RebaseTimer(_) => {
                    self.reply(seq, VcpuResponse::Error("not paused".into()));
                }
            }
        }
    }

    /// Blocks a paused vCPU until Resume, serving state requests meanwhile.
    /// Returns false if the channel was closed.
    fn park(&mut self, hvf_vcpu: &mut HvfVcpu) -> bool {
        loop {
            let Ok(VcpuRequest { seq, event }) = self.event_receiver.recv() else {
                return false;
            };
            let response = match event {
                VcpuEvent::Resume => {
                    debug!("vcpu.debug paused.resume_ack vcpu={}", hvf_vcpu.id());
                    self.reply(seq, VcpuResponse::Resumed);
                    return true;
                }
                // Already paused — re-acknowledge with current state.
                VcpuEvent::Pause => paused_response(hvf_vcpu),
                VcpuEvent::RestoreState(bytes) => {
                    match bincode::deserialize::<hvf::state::HvfVcpuState>(&bytes) {
                        Ok(st) => match hvf_vcpu.restore_state(&st) {
                            Ok(()) => VcpuResponse::Restored,
                            Err(e) => VcpuResponse::Error(format!("restore: {e}")),
                        },
                        Err(e) => VcpuResponse::Error(format!("decode: {e}")),
                    }
                }
                VcpuEvent::RestoreKvmState {
                    state,
                    restore_counter,
                    gic,
                } => match restore_kvm_state(hvf_vcpu, &state, restore_counter, gic.as_ref()) {
                    Ok(()) => VcpuResponse::Restored,
                    Err(e) => VcpuResponse::Error(format!("restore kvm: {e}")),
                },
                VcpuEvent::RestoreGicRedist(regs) => {
                    match hvf_vcpu.restore_gic_redist_regs(&regs) {
                        Ok(()) => VcpuResponse::Restored,
                        Err(e) => VcpuResponse::Error(format!("restore redist: {e}")),
                    }
                }
                // Rebasing on this, the owning, thread is all restore needs.
                // The in-kernel GIC delivers the vtimer itself. With the
                // userspace GIC, rebase_timer leaves the vtimer unmasked, so
                // hv_vcpu_run reports VTIMER_ACTIVATED once it fires and a
                // guest WFI derives its own wait deadline from CNTV_CVAL_EL0.
                VcpuEvent::RebaseTimer(delta) => match hvf_vcpu.rebase_timer(delta) {
                    Ok(()) => VcpuResponse::TimerRebased,
                    Err(e) => VcpuResponse::Error(format!("rebase: {e}")),
                },
            };
            self.reply(seq, response);
        }
    }

    /// A closed channel means the handle is gone; the next receive on the
    /// request channel observes that, so a failed send needs no handling.
    fn reply(&self, seq: u64, response: VcpuResponse) {
        let _ = self
            .response_sender
            .send(VcpuReport::Reply { seq, response });
    }

    /// Parks this vCPU until a device IRQ arrives on `receiver`, the optional
    /// vtimer `timeout` elapses, or a control event is queued. Watching the
    /// control channel is load-bearing: an idle vCPU blocks here rather than
    /// in hv_vcpu_run, so hv_vcpus_exit cannot break it out, and without it a
    /// Pause would never be seen. The control event stays queued for
    /// process_control_events at the top of the run loop.
    fn wait_for_event(
        &mut self,
        hvf_vcpuid: u64,
        receiver: &Receiver<()>,
        timeout: Option<Duration>,
    ) {
        if !self.vcpu_list.should_wait(hvf_vcpuid) {
            return;
        }
        debug!("vcpu.debug wait_for_event.enter vcpu={hvf_vcpuid} timeout={timeout:?}");
        let mut select = Select::new();
        let wfe = select.recv(receiver);
        select.recv(&self.event_receiver);
        let ready = match timeout {
            Some(timeout) => select.ready_timeout(timeout).ok(),
            None => Some(select.ready()),
        };
        match ready {
            Some(index) if index == wfe => match receiver.try_recv() {
                Err(TryRecvError::Disconnected) => panic!("WFE channel closed unexpectedly"),
                Ok(_) | Err(TryRecvError::Empty) => {
                    debug!("vcpu.debug wait_for_event.wake vcpu={hvf_vcpuid}")
                }
            },
            Some(_) => debug!("vcpu.debug wait_for_event.control vcpu={hvf_vcpuid}"),
            None => debug!("vcpu.debug wait_for_event.timeout vcpu={hvf_vcpuid}"),
        }
    }

    fn wait_for_resume(&mut self) {}

    fn exit(&mut self, exit_code: u8) {
        self.response_sender
            .send(VcpuReport::Exited(exit_code))
            .expect("failed to send Exited status");

        if let Err(e) = self.exit_evt.write(1) {
            error!("Failed signaling vcpu exit event: {e}");
        }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        let _ = self.reset_thread_local_data();
    }
}

fn paused_response(hvf_vcpu: &HvfVcpu) -> VcpuResponse {
    match hvf_vcpu
        .save_state()
        .and_then(|s| bincode::serialize(&s).map_err(|_| hvf::Error::VcpuReadRegister))
    {
        Ok(payload) => VcpuResponse::Paused(payload),
        Err(e) => VcpuResponse::Error(format!("save_state: {e}")),
    }
}

#[derive(Debug)]
/// List of events that the Vcpu can receive.
pub enum VcpuEvent {
    /// Pause the Vcpu. While paused, the vCPU thread waits on the event channel
    /// rather than running HVF.
    Pause,
    /// Event that should resume the Vcpu.
    Resume,
    /// Apply a captured HvfVcpuState (bincode-encoded) — only legal while paused.
    RestoreState(Vec<u8>),
    /// Apply a captured Linux/KVM aarch64 vCPU state while paused.
    RestoreKvmState {
        state: Vec<u8>,
        restore_counter: u64,
        gic: Option<KvmGicVcpuState>,
    },
    /// Apply GIC redistributor registers on the owning vCPU thread.
    RestoreGicRedist(Vec<(u32, u64)>),
    /// Re-arm virtual timer state after snapshot restore — only legal while paused.
    RebaseTimer(u64),
}

#[derive(Debug)]
/// List of responses that the Vcpu reports.
pub enum VcpuResponse {
    /// Vcpu is paused. Payload is bincode-encoded HvfVcpuState.
    Paused(Vec<u8>),
    /// Vcpu is resumed.
    Resumed,
    /// RestoreState applied.
    Restored,
    /// RebaseTimer applied.
    TimerRebased,
    /// A control event failed.
    Error(String),
}

/// Sequence number of the Pause that `Vcpu::queue_initial_pause` queues
/// before the handle exists. Requests sent through the handle start after it.
const INITIAL_PAUSE_SEQ: u64 = 0;

/// A control event tagged with the sequence number its reply will carry.
#[derive(Debug)]
pub struct VcpuRequest {
    seq: u64,
    event: VcpuEvent,
}

/// What a vCPU thread sends back to its handle.
#[derive(Debug)]
pub enum VcpuReport {
    /// Reply to the request with sequence number `seq`.
    Reply { seq: u64, response: VcpuResponse },
    /// The vCPU stopped running the guest. Unsolicited.
    Exited(u8),
}

/// Identifies one sent request so its reply can be told apart from replies
/// to earlier requests that were abandoned (e.g. after a timeout).
#[must_use]
#[derive(Debug)]
pub struct VcpuTicket(u64);

#[derive(Debug)]
pub enum ReplyError {
    /// No reply arrived in time. A later reply to this request is discarded
    /// by whichever wait comes next.
    Timeout,
    /// The vCPU exited before replying.
    Exited(u8),
    /// The vCPU thread is gone.
    Disconnected,
}

impl Display for ReplyError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            ReplyError::Timeout => write!(f, "timed out waiting for reply"),
            ReplyError::Exited(code) => write!(f, "vCPU exited with code {code}"),
            ReplyError::Disconnected => write!(f, "vCPU thread is gone"),
        }
    }
}

/// Wrapper over Vcpu that hides the underlying interactions with the Vcpu thread.
pub struct VcpuHandle {
    event_sender: Sender<VcpuRequest>,
    response_receiver: Receiver<VcpuReport>,
    next_seq: AtomicU64,
    exit_code: OnceLock<u8>,
}

impl VcpuHandle {
    pub fn new(
        event_sender: Sender<VcpuRequest>,
        response_receiver: Receiver<VcpuReport>,
        _vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            event_sender,
            response_receiver,
            next_seq: AtomicU64::new(INITIAL_PAUSE_SEQ + 1),
            exit_code: OnceLock::new(),
        }
    }

    /// Queues `event` for the vCPU thread. The vCPU only notices it between
    /// guest runs or while idle; callers kick a running vCPU out of the guest.
    pub fn send_event(&self, event: VcpuEvent) -> Result<VcpuTicket> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        // The guest can shut down at any time, e.g. while a snapshot is
        // pausing or resuming it, so a gone vCPU is an error, not a bug.
        self.event_sender
            .send(VcpuRequest { seq, event })
            .map_err(|_| Error::VcpuThreadGone)?;
        Ok(VcpuTicket(seq))
    }

    /// Waits for the reply to the request identified by `ticket`, discarding
    /// replies to earlier requests that were abandoned.
    pub fn wait_reply(
        &self,
        ticket: VcpuTicket,
        timeout: Duration,
    ) -> result::Result<VcpuResponse, ReplyError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.response_receiver.recv_deadline(deadline) {
                Ok(VcpuReport::Reply { seq, response }) if seq == ticket.0 => return Ok(response),
                Ok(VcpuReport::Reply { seq, response }) => {
                    debug!(
                        "vcpu.debug stale_reply seq={seq} waiting={} {response:?}",
                        ticket.0
                    );
                }
                Ok(VcpuReport::Exited(code)) => {
                    let _ = self.exit_code.set(code);
                    return Err(ReplyError::Exited(code));
                }
                Err(RecvTimeoutError::Timeout) => return Err(ReplyError::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(ReplyError::Disconnected),
            }
        }
    }

    /// Ticket for the Pause that `Vcpu::queue_initial_pause` queued before
    /// this handle existed.
    pub fn initial_pause_ticket(&self) -> VcpuTicket {
        VcpuTicket(INITIAL_PAUSE_SEQ)
    }

    /// The code the vCPU exited with, if it has exited. Replies still queued
    /// are discarded: once a vCPU has exited nobody waits for them.
    pub fn exit_code(&self) -> Option<u8> {
        while let Ok(report) = self.response_receiver.try_recv() {
            if let VcpuReport::Exited(code) = report {
                let _ = self.exit_code.set(code);
            }
        }
        self.exit_code.get().copied()
    }
}

/// Subset of microvm state the snapshot orchestrator needs, populated when
/// the Vmm is built.
pub struct SnapshotCtx {
    pub vcpu_ids: Vec<u64>,
    pub vcpu_list: Arc<devices::legacy::VcpuList>,
    pub irqchip: devices::legacy::IrqChip,
    pub gic: Option<Arc<std::sync::Mutex<devices::legacy::GicV3>>>,
    pub nested_enabled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct KvmGicVcpuState {
    pub icc_regs: Vec<(u16, u64)>,
    pub redist_regs: Vec<(u32, u64)>,
    pub ich_regs: Vec<(u16, u64)>,
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Debug, Deserialize)]
struct KvmAarch64OneRegCompat {
    id: u64,
    value: Vec<u8>,
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Debug, Deserialize)]
struct KvmVcpuStateCompat {
    regs: Vec<KvmAarch64OneRegCompat>,
    #[serde(default)]
    mp_state: Option<Vec<u8>>,
    #[serde(default)]
    vcpu_events: Option<Vec<u8>>,
}

#[cfg(target_arch = "aarch64")]
fn restore_kvm_state(
    hvf_vcpu: &mut HvfVcpu,
    bytes: &[u8],
    restore_counter: u64,
    gic: Option<&KvmGicVcpuState>,
) -> std::result::Result<(), String> {
    let kvm_state =
        bincode::deserialize::<KvmVcpuStateCompat>(bytes).map_err(|e| format!("decode: {e}"))?;
    let hvf_state = kvm_state_to_hvf_state(&kvm_state, restore_counter, gic)?;
    hvf_vcpu
        .restore_state(&hvf_state)
        .map_err(|e| format!("restore: {e:?}"))
}

#[cfg(not(target_arch = "aarch64"))]
fn restore_kvm_state(
    _hvf_vcpu: &mut HvfVcpu,
    _bytes: &[u8],
    _restore_counter: u64,
    _gic: Option<&KvmGicVcpuState>,
) -> std::result::Result<(), String> {
    Err("KVM aarch64 state restore is unsupported on this architecture".to_string())
}

#[cfg(target_arch = "aarch64")]
fn kvm_state_to_hvf_state(
    state: &KvmVcpuStateCompat,
    restore_counter: u64,
    gic: Option<&KvmGicVcpuState>,
) -> std::result::Result<hvf::state::HvfVcpuState, String> {
    let _ = (&state.mp_state, &state.vcpu_events);

    let mut gp = [0u64; 31];
    for (index, slot) in gp.iter_mut().enumerate() {
        *slot = required_kvm_reg_u64(state, core_user_reg_id(index))?;
    }

    let pc = required_kvm_reg_u64(state, core_user_pc_id())?;
    let cpsr = required_kvm_reg_u64(state, core_user_pstate_id())?;
    let fpsr = required_kvm_reg_u32(state, core_fp_fpsr_id())? as u64;
    let fpcr = required_kvm_reg_u32(state, core_fp_fpcr_id())? as u64;
    let mut fp = [0u128; 32];
    for (index, slot) in fp.iter_mut().enumerate() {
        *slot = required_kvm_reg_u128(state, core_fp_vreg_id(index))?;
    }

    let mut sysregs = Vec::new();
    push_required_core_sysreg(
        state,
        &mut sysregs,
        core_spsr_id(0),
        hvf_sys_reg(3, 0, 4, 0, 0),
    )?;
    push_required_core_sysreg(
        state,
        &mut sysregs,
        core_elr_el1_id(),
        hvf_sys_reg(3, 0, 4, 0, 1),
    )?;
    push_required_core_sysreg(
        state,
        &mut sysregs,
        core_user_sp_id(),
        hvf_sys_reg(3, 0, 4, 1, 0),
    )?;
    push_required_core_sysreg(
        state,
        &mut sysregs,
        core_sp_el1_id(),
        hvf_sys_reg(3, 4, 4, 1, 0),
    )?;

    let mut saved_counter = None;
    for reg in &state.regs {
        if !kvm_reg_is_sysreg(reg.id) {
            continue;
        }
        let value = kvm_reg_value_u64(reg)?;
        if reg.id == kvm_timer_counter_id() {
            saved_counter = Some(value);
            continue;
        }
        if reg.id == kvm_timer_cval_id() {
            sysregs.push((hvf_sys_reg(3, 3, 14, 3, 2), value));
            continue;
        }
        // KVM lists registers HVF does not have (PMU, debug, AArch32 ID
        // registers, ...). Carry over exactly what an HVF snapshot would hold,
        // so restore can treat every remaining register as required.
        let hvf_reg = kvm_sysreg_to_hvf_sysreg(reg.id);
        if hvf::state::is_snapshot_sys_reg(hvf_reg) {
            sysregs.push((hvf_reg, value));
        }
    }

    let vtimer_offset = saved_counter
        .map(|counter| restore_counter.wrapping_sub(counter))
        .unwrap_or(0);

    let (gic_icc_regs, gic_redist_regs, gic_ich_regs) = gic
        .map(|state| {
            (
                state.icc_regs.clone(),
                state.redist_regs.clone(),
                state.ich_regs.clone(),
            )
        })
        .unwrap_or_default();
    debug!(
        "hvf.kvm_state.translated pc=0x{pc:x} cpsr=0x{cpsr:x} vtimer_offset=0x{vtimer_offset:x} sysregs={} gic_icc={} gic_redist={} gic_ich={}",
        sysregs.len(),
        gic_icc_regs.len(),
        gic_redist_regs.len(),
        gic_ich_regs.len()
    );

    Ok(hvf::state::HvfVcpuState {
        gp,
        pc,
        cpsr,
        fpcr,
        fpsr,
        fp,
        sysregs,
        gic_icc_regs,
        gic_redist_regs,
        gic_ich_regs,
        vtimer_masked: false,
        vtimer_offset,
    })
}

#[cfg(target_arch = "aarch64")]
fn push_required_core_sysreg(
    state: &KvmVcpuStateCompat,
    sysregs: &mut Vec<(u16, u64)>,
    kvm_id: u64,
    hvf_id: u16,
) -> std::result::Result<(), String> {
    sysregs.push((hvf_id, required_kvm_reg_u64(state, kvm_id)?));
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn required_kvm_reg_u64(state: &KvmVcpuStateCompat, id: u64) -> std::result::Result<u64, String> {
    let reg = state
        .regs
        .iter()
        .find(|reg| reg.id == id)
        .ok_or_else(|| format!("missing KVM reg 0x{id:x}"))?;
    kvm_reg_value_u64(reg)
}

#[cfg(target_arch = "aarch64")]
fn required_kvm_reg_u32(state: &KvmVcpuStateCompat, id: u64) -> std::result::Result<u32, String> {
    let reg = state
        .regs
        .iter()
        .find(|reg| reg.id == id)
        .ok_or_else(|| format!("missing KVM reg 0x{id:x}"))?;
    let bytes = reg
        .value
        .get(..4)
        .ok_or_else(|| format!("short KVM reg 0x{id:x}"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(target_arch = "aarch64")]
fn required_kvm_reg_u128(state: &KvmVcpuStateCompat, id: u64) -> std::result::Result<u128, String> {
    let reg = state
        .regs
        .iter()
        .find(|reg| reg.id == id)
        .ok_or_else(|| format!("missing KVM reg 0x{id:x}"))?;
    let bytes = reg
        .value
        .get(..16)
        .ok_or_else(|| format!("short KVM reg 0x{id:x}"))?;
    Ok(u128::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(target_arch = "aarch64")]
fn kvm_reg_value_u64(reg: &KvmAarch64OneRegCompat) -> std::result::Result<u64, String> {
    let bytes = reg
        .value
        .get(..8)
        .ok_or_else(|| format!("short KVM reg 0x{:x}", reg.id))?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(target_arch = "aarch64")]
fn kvm_reg_is_sysreg(id: u64) -> bool {
    (id & KVM_REG_ARM_COPROC_MASK) == KVM_REG_ARM64_SYSREG
}

#[cfg(target_arch = "aarch64")]
fn kvm_sysreg_to_hvf_sysreg(id: u64) -> u16 {
    hvf_sys_reg(
        ((id & KVM_REG_ARM64_SYSREG_OP0_MASK) >> KVM_REG_ARM64_SYSREG_OP0_SHIFT) as u16,
        ((id & KVM_REG_ARM64_SYSREG_OP1_MASK) >> KVM_REG_ARM64_SYSREG_OP1_SHIFT) as u16,
        ((id & KVM_REG_ARM64_SYSREG_CRN_MASK) >> KVM_REG_ARM64_SYSREG_CRN_SHIFT) as u16,
        ((id & KVM_REG_ARM64_SYSREG_CRM_MASK) >> KVM_REG_ARM64_SYSREG_CRM_SHIFT) as u16,
        ((id & KVM_REG_ARM64_SYSREG_OP2_MASK) >> KVM_REG_ARM64_SYSREG_OP2_SHIFT) as u16,
    )
}

#[cfg(target_arch = "aarch64")]
fn core_reg_id(offset: usize, size: u64) -> u64 {
    KVM_REG_ARM64 | size | KVM_REG_ARM_CORE | (offset / std::mem::size_of::<u32>()) as u64
}

#[cfg(target_arch = "aarch64")]
fn core_user_reg_id(index: usize) -> u64 {
    core_reg_id(
        KVM_REGS_REGS_OFFSET + USER_PT_REGS_REGS_OFFSET + index * 8,
        KVM_REG_SIZE_U64,
    )
}

#[cfg(target_arch = "aarch64")]
fn core_user_sp_id() -> u64 {
    core_reg_id(
        KVM_REGS_REGS_OFFSET + USER_PT_REGS_SP_OFFSET,
        KVM_REG_SIZE_U64,
    )
}

#[cfg(target_arch = "aarch64")]
fn core_user_pc_id() -> u64 {
    core_reg_id(
        KVM_REGS_REGS_OFFSET + USER_PT_REGS_PC_OFFSET,
        KVM_REG_SIZE_U64,
    )
}

#[cfg(target_arch = "aarch64")]
fn core_user_pstate_id() -> u64 {
    core_reg_id(
        KVM_REGS_REGS_OFFSET + USER_PT_REGS_PSTATE_OFFSET,
        KVM_REG_SIZE_U64,
    )
}

#[cfg(target_arch = "aarch64")]
fn core_sp_el1_id() -> u64 {
    core_reg_id(KVM_REGS_SP_EL1_OFFSET, KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_elr_el1_id() -> u64 {
    core_reg_id(KVM_REGS_ELR_EL1_OFFSET, KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_spsr_id(index: usize) -> u64 {
    core_reg_id(KVM_REGS_SPSR_OFFSET + index * 8, KVM_REG_SIZE_U64)
}

#[cfg(target_arch = "aarch64")]
fn core_fp_vreg_id(index: usize) -> u64 {
    core_reg_id(
        KVM_REGS_FP_REGS_OFFSET + USER_FPSIMD_VREGS_OFFSET + index * 16,
        KVM_REG_SIZE_U128,
    )
}

#[cfg(target_arch = "aarch64")]
fn core_fp_fpsr_id() -> u64 {
    core_reg_id(
        KVM_REGS_FP_REGS_OFFSET + USER_FPSIMD_FPSR_OFFSET,
        KVM_REG_SIZE_U32,
    )
}

#[cfg(target_arch = "aarch64")]
fn core_fp_fpcr_id() -> u64 {
    core_reg_id(
        KVM_REGS_FP_REGS_OFFSET + USER_FPSIMD_FPCR_OFFSET,
        KVM_REG_SIZE_U32,
    )
}

#[cfg(target_arch = "aarch64")]
const fn hvf_sys_reg(op0: u16, op1: u16, crn: u16, crm: u16, op2: u16) -> u16 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

#[cfg(target_arch = "aarch64")]
fn kvm_timer_cval_id() -> u64 {
    arm64_sys_reg_id(3, 3, 14, 0, 2)
}

#[cfg(target_arch = "aarch64")]
fn kvm_timer_counter_id() -> u64 {
    arm64_sys_reg_id(3, 3, 14, 3, 2)
}

#[cfg(target_arch = "aarch64")]
fn arm64_sys_reg_id(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    KVM_REG_ARM64
        | KVM_REG_SIZE_U64
        | KVM_REG_ARM64_SYSREG
        | ((op0 << KVM_REG_ARM64_SYSREG_OP0_SHIFT) & KVM_REG_ARM64_SYSREG_OP0_MASK)
        | ((op1 << KVM_REG_ARM64_SYSREG_OP1_SHIFT) & KVM_REG_ARM64_SYSREG_OP1_MASK)
        | ((crn << KVM_REG_ARM64_SYSREG_CRN_SHIFT) & KVM_REG_ARM64_SYSREG_CRN_MASK)
        | ((crm << KVM_REG_ARM64_SYSREG_CRM_SHIFT) & KVM_REG_ARM64_SYSREG_CRM_MASK)
        | ((op2 << KVM_REG_ARM64_SYSREG_OP2_SHIFT) & KVM_REG_ARM64_SYSREG_OP2_MASK)
}

#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64: u64 = 0x6000_0000_0000_0000;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM_COPROC_MASK: u64 = 0x0fff_0000;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM_CORE: u64 = 0x0010_0000;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG: u64 = 0x0013_0000;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_OP0_MASK: u64 = 0x0000_c000;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_OP0_SHIFT: u64 = 14;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_OP1_MASK: u64 = 0x0000_3800;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_OP1_SHIFT: u64 = 11;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_CRN_MASK: u64 = 0x0000_0780;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_CRN_SHIFT: u64 = 7;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_CRM_MASK: u64 = 0x0000_0078;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_CRM_SHIFT: u64 = 3;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_OP2_MASK: u64 = 0x0000_0007;
#[cfg(target_arch = "aarch64")]
const KVM_REG_ARM64_SYSREG_OP2_SHIFT: u64 = 0;
#[cfg(target_arch = "aarch64")]
const KVM_REG_SIZE_U32: u64 = 0x0020_0000_0000_0000;
#[cfg(target_arch = "aarch64")]
const KVM_REG_SIZE_U64: u64 = 0x0030_0000_0000_0000;
#[cfg(target_arch = "aarch64")]
const KVM_REG_SIZE_U128: u64 = 0x0040_0000_0000_0000;

#[cfg(target_arch = "aarch64")]
const KVM_REGS_REGS_OFFSET: usize = 0;
#[cfg(target_arch = "aarch64")]
const KVM_REGS_SP_EL1_OFFSET: usize = 272;
#[cfg(target_arch = "aarch64")]
const KVM_REGS_ELR_EL1_OFFSET: usize = 280;
#[cfg(target_arch = "aarch64")]
const KVM_REGS_SPSR_OFFSET: usize = 288;
#[cfg(target_arch = "aarch64")]
const KVM_REGS_FP_REGS_OFFSET: usize = 336;
#[cfg(target_arch = "aarch64")]
const USER_PT_REGS_REGS_OFFSET: usize = 0;
#[cfg(target_arch = "aarch64")]
const USER_PT_REGS_SP_OFFSET: usize = 248;
#[cfg(target_arch = "aarch64")]
const USER_PT_REGS_PC_OFFSET: usize = 256;
#[cfg(target_arch = "aarch64")]
const USER_PT_REGS_PSTATE_OFFSET: usize = 264;
#[cfg(target_arch = "aarch64")]
const USER_FPSIMD_VREGS_OFFSET: usize = 0;
#[cfg(target_arch = "aarch64")]
const USER_FPSIMD_FPSR_OFFSET: usize = 512;
#[cfg(target_arch = "aarch64")]
const USER_FPSIMD_FPCR_OFFSET: usize = 516;

enum VcpuEmulation {
    Handled,
    Interrupted,
    Stopped,
    WaitForEvent,
    WaitForEventExpired,
    WaitForEventTimeout(Duration),
}

fn debug_log_vcpu_exit(vcpuid: u64, kind: String) {
    if VCPU_EXIT_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed) < 200 {
        debug!("vcpu.debug hvf_exit vcpu={vcpuid} {kind}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use std::ffi::CString;

    use super::*;
    use arch::aarch64::layout::DRAM_MEM_START_EFI;
    use devices::legacy::VcpuList;
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    #[cfg(target_arch = "aarch64")]
    fn one_reg_u64(id: u64, value: u64) -> KvmAarch64OneRegCompat {
        KvmAarch64OneRegCompat {
            id,
            value: value.to_le_bytes().to_vec(),
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn one_reg_u32(id: u64, value: u32) -> KvmAarch64OneRegCompat {
        KvmAarch64OneRegCompat {
            id,
            value: value.to_le_bytes().to_vec(),
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn one_reg_u128(id: u64, value: u128) -> KvmAarch64OneRegCompat {
        KvmAarch64OneRegCompat {
            id,
            value: value.to_le_bytes().to_vec(),
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn sysreg_value(state: &hvf::state::HvfVcpuState, reg: u16) -> u64 {
        state
            .sysregs
            .iter()
            .find_map(|&(candidate, value)| (candidate == reg).then_some(value))
            .expect("missing sysreg")
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn kvm_state_translation_maps_core_fp_sysregs_and_timer() {
        let writable_sysreg = arm64_sys_reg_id(3, 0, 1, 0, 0);
        let pmcr_el0 = arm64_sys_reg_id(3, 3, 9, 12, 0);
        let mut regs = Vec::new();
        for index in 0..31 {
            regs.push(one_reg_u64(core_user_reg_id(index), 0x1000 + index as u64));
        }
        regs.extend([
            one_reg_u64(core_user_sp_id(), 0x2000),
            one_reg_u64(core_user_pc_id(), 0x3000),
            one_reg_u64(core_user_pstate_id(), 0x3c5),
            one_reg_u64(core_spsr_id(0), 0x4000),
            one_reg_u64(core_elr_el1_id(), 0x5000),
            one_reg_u64(core_sp_el1_id(), 0x6000),
            one_reg_u32(core_fp_fpsr_id(), 0x77),
            one_reg_u32(core_fp_fpcr_id(), 0x88),
            one_reg_u128(
                core_fp_vreg_id(0),
                0x1111_2222_3333_4444_5555_6666_7777_8888,
            ),
            one_reg_u64(writable_sysreg, 0x9000),
            one_reg_u64(pmcr_el0, 0x41),
            one_reg_u64(kvm_timer_cval_id(), 0xa000),
            one_reg_u64(kvm_timer_counter_id(), 0xb000),
        ]);
        for index in 1..32 {
            regs.push(one_reg_u128(core_fp_vreg_id(index), index as u128));
        }

        let kvm = KvmVcpuStateCompat {
            regs,
            mp_state: Some(vec![0; 4]),
            vcpu_events: Some(vec![1, 2, 3]),
        };

        let gic = KvmGicVcpuState {
            icc_regs: vec![(hvf_sys_reg(3, 0, 4, 6, 0), 0xf0)],
            redist_regs: vec![(0x1_0100, 0xffff_ffff)],
            ich_regs: vec![(hvf_sys_reg(3, 4, 12, 12, 0), 0x8000_0000_0000_0045)],
        };
        let hvf = kvm_state_to_hvf_state(&kvm, 0xf000, Some(&gic)).expect("translate");

        assert_eq!(hvf.gp[0], 0x1000);
        assert_eq!(hvf.gp[30], 0x101e);
        assert_eq!(hvf.pc, 0x3000);
        assert_eq!(hvf.cpsr, 0x3c5);
        assert_eq!(hvf.fpsr, 0x77);
        assert_eq!(hvf.fpcr, 0x88);
        assert_eq!(hvf.fp[0], 0x1111_2222_3333_4444_5555_6666_7777_8888);
        assert_eq!(sysreg_value(&hvf, hvf_sys_reg(3, 0, 4, 0, 0)), 0x4000);
        assert_eq!(sysreg_value(&hvf, hvf_sys_reg(3, 0, 4, 0, 1)), 0x5000);
        assert_eq!(sysreg_value(&hvf, hvf_sys_reg(3, 0, 4, 1, 0)), 0x2000);
        assert_eq!(sysreg_value(&hvf, hvf_sys_reg(3, 4, 4, 1, 0)), 0x6000);
        assert_eq!(sysreg_value(&hvf, hvf_sys_reg(3, 0, 1, 0, 0)), 0x9000);
        assert_eq!(sysreg_value(&hvf, hvf_sys_reg(3, 3, 14, 3, 2)), 0xa000);
        assert_eq!(hvf.vtimer_offset, 0x4000);
        assert_eq!(hvf.gic_icc_regs, gic.icc_regs);
        assert_eq!(hvf.gic_redist_regs, gic.redist_regs);
        assert_eq!(hvf.gic_ich_regs, gic.ich_regs);
        assert!(
            !hvf.sysregs
                .iter()
                .any(|&(reg, _)| reg == hvf_sys_reg(3, 3, 9, 12, 0)),
            "PMCR_EL0 is not an HVF snapshot register"
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn kvm_state_translation_requires_core_registers() {
        let kvm = KvmVcpuStateCompat {
            regs: vec![one_reg_u64(core_user_pc_id(), 0)],
            mp_state: None,
            vcpu_events: None,
        };

        assert!(kvm_state_to_hvf_state(&kvm, 0, None).is_err());
    }

    // Auxiliary function being used throughout the tests.
    // Does NOT create a real HVF VM — Vcpu::new_aarch64 and most vcpu methods
    // work without one, keeping tests free from the one-VM-per-process limit.
    fn setup_vcpu(mem_size: usize) -> (Vcpu, GuestMemoryMmap) {
        let gm = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)]).unwrap();
        let exit_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap();
        let vcpu_list = Arc::new(VcpuList::new(1));
        let vcpu = Vcpu::new_aarch64(
            1,
            GuestAddress(0),
            None,
            exit_evt,
            vcpu_list,
            false,
            InterruptController::Userspace,
        )
        .unwrap();
        (vcpu, gm)
    }

    #[test]
    fn test_set_mmio_bus() {
        let (mut vcpu, _) = setup_vcpu(0x1000);
        assert!(vcpu.mmio_bus.is_none());
        vcpu.set_mmio_bus(devices::Bus::new());
        assert!(vcpu.mmio_bus.is_some());
    }

    // CI macOS runners are VMs without Hypervisor.framework; every other test
    // in this module avoids creating a real HVF VM, but this one cannot.
    fn hvf_available() -> bool {
        let name = CString::new("kern.hv_support").unwrap();
        let mut value: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let ret = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut value as *mut libc::c_int as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        ret == 0 && value == 1
    }

    /// Hypervisor.framework allows one VM per process and libkrun never
    /// destroys it, so every test that needs a real VM shares this one.
    struct SharedTestVm {
        vm: Vm,
        vcpu_list: Arc<VcpuList>,
        /// `None` when the host has no in-kernel GIC (macOS before 15).
        in_kernel_gic: Option<devices::legacy::HvfGicV3>,
        /// Host memory backing guest mappings; it must outlive the VM.
        guest_memory: Vec<GuestMemoryMmap>,
    }

    fn shared_test_vm() -> std::sync::MutexGuard<'static, SharedTestVm> {
        static VM: std::sync::OnceLock<std::sync::Mutex<SharedTestVm>> = std::sync::OnceLock::new();
        VM.get_or_init(|| {
            let vm = Vm::new(false).expect("Cannot create new vm");
            let vcpu_list = Arc::new(VcpuList::new(1));
            // The in-kernel GIC has to exist before any vCPU is created.
            let in_kernel_gic = devices::legacy::HvfGicV3::new(1, vcpu_list.clone()).ok();
            std::sync::Mutex::new(SharedTestVm {
                vm,
                vcpu_list,
                in_kernel_gic,
                guest_memory: Vec::new(),
            })
        })
        .lock()
        .unwrap()
    }

    #[test]
    fn test_vm_memory_init() {
        if !hvf_available() {
            eprintln!("skipping test_vm_memory_init: host has no Hypervisor.framework support");
            return;
        }
        let mut shared = shared_test_vm();

        // Use a realistic guest physical address; hv_vm_map rejects GPA 0.
        let gm = GuestMemoryMmap::from_ranges(&[(
            GuestAddress(DRAM_MEM_START_EFI),
            0x20_0000, // 2 MB
        )])
        .unwrap();
        shared.vm.memory_init(&gm, &[]).expect("memory_init failed");
        shared.guest_memory.push(gm);
    }

    fn set_sysreg(sysregs: &mut [(u16, u64)], reg: u16, value: u64) {
        let entry = sysregs
            .iter_mut()
            .find(|(id, _)| *id == reg)
            .unwrap_or_else(|| panic!("vCPU state has no sysreg {reg:#x}"));
        entry.1 = value;
    }

    fn reply(handle: &VcpuHandle, ticket: VcpuTicket) -> VcpuResponse {
        handle
            .wait_reply(ticket, Duration::from_secs(10))
            .expect("vCPU thread did not reply")
    }

    fn call(handle: &VcpuHandle, event: VcpuEvent) -> VcpuResponse {
        reply(handle, handle.send_event(event).unwrap())
    }

    /// Snapshot restore re-arms the vtimer with RebaseTimer. Under the
    /// in-kernel GIC, HVF owns vtimer delivery: nothing may be queued for
    /// userspace injection, which macOS 27 rejects with HV_UNSUPPORTED.
    #[test]
    fn rebase_timer_with_in_kernel_gic_queues_no_userspace_irq() {
        if !hvf_available() {
            eprintln!("skipping: host has no Hypervisor.framework support");
            return;
        }
        let mut shared = shared_test_vm();
        if shared.in_kernel_gic.is_none() {
            eprintln!("skipping: host has no in-kernel HVF GIC");
            return;
        }

        // Spin long enough for any out-of-band vtimer injection to land while
        // the guest runs, then power off through PSCI SYSTEM_OFF.
        const GUEST_CODE: [u32; 7] = [
            0xd2a0_4001, // movz x1, #0x200, lsl #16
            0xf100_0421, // subs x1, x1, #1
            0x54ff_ffe1, // b.ne .-4
            0xd280_0100, // movz x0, #0x8
            0xf2b0_8000, // movk x0, #0x8400, lsl #16
            0xd400_0002, // hvc #0
            0x1400_0000, // b .
        ];
        let code_addr = GuestAddress(DRAM_MEM_START_EFI + 0x100_0000);
        let gm = GuestMemoryMmap::from_ranges(&[(code_addr, 0x20_0000)]).unwrap();
        let code: Vec<u8> = GUEST_CODE.iter().flat_map(|i| i.to_le_bytes()).collect();
        vm_memory::Bytes::write_slice(&gm, &code, code_addr).unwrap();
        shared.vm.memory_init(&gm, &[]).expect("memory_init failed");
        shared.guest_memory.push(gm);

        let mut vcpu = Vcpu::new_aarch64(
            0,
            code_addr,
            None,
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            shared.vcpu_list.clone(),
            false,
            InterruptController::InKernel,
        )
        .unwrap();
        vcpu.queue_initial_pause();
        let handle = vcpu.start_threaded().unwrap();

        // Restore a vCPU whose vtimer is enabled and already expired.
        let VcpuResponse::Paused(payload) = reply(&handle, handle.initial_pause_ticket()) else {
            panic!("expected the initial pause");
        };
        let mut state: hvf::state::HvfVcpuState = bincode::deserialize(&payload).unwrap();
        set_sysreg(
            &mut state.sysregs,
            hvf::bindings::hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0,
            0,
        );
        set_sysreg(
            &mut state.sysregs,
            hvf::bindings::hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0,
            1,
        );
        let response = call(
            &handle,
            VcpuEvent::RestoreState(bincode::serialize(&state).unwrap()),
        );
        assert!(matches!(response, VcpuResponse::Restored), "{response:?}");

        let response = call(&handle, VcpuEvent::RebaseTimer(0));
        assert!(
            matches!(response, VcpuResponse::TimerRebased),
            "{response:?}"
        );

        let response = call(&handle, VcpuEvent::Resume);
        assert!(matches!(response, VcpuResponse::Resumed), "{response:?}");

        let report = handle
            .response_receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("vCPU thread did not exit");
        assert!(
            matches!(report, VcpuReport::Exited(FC_EXIT_CODE_OK)),
            "{report:?}"
        );
        assert!(!shared.vcpu_list.has_pending_irq(0));
    }

    #[test]
    fn test_configure_vcpu() {
        // configure_aarch64 only sets fdt_addr — no HVF VM needed.
        let mem_info = arch::ArchMemoryInfo::default();

        // Try it for when vcpu id is 0.
        let vcpu_list = Arc::new(VcpuList::new(1));
        let mut vcpu = Vcpu::new_aarch64(
            0,
            GuestAddress(0),
            None,
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            vcpu_list,
            false,
            InterruptController::Userspace,
        )
        .unwrap();
        assert!(vcpu.configure_aarch64(&mem_info).is_ok());

        // Try it for when vcpu id is NOT 0.
        let vcpu_list = Arc::new(VcpuList::new(2));
        let mut vcpu = Vcpu::new_aarch64(
            1,
            GuestAddress(0),
            None,
            EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap(),
            vcpu_list,
            false,
            InterruptController::Userspace,
        )
        .unwrap();
        assert!(vcpu.configure_aarch64(&mem_info).is_ok());
    }

    #[test]
    fn test_vcpu_tls() {
        let (mut vcpu, _) = setup_vcpu(0x1000);

        // Reset should fail before TLS is initialized.
        assert!(vcpu.reset_thread_local_data().is_err());

        // Initialize vcpu TLS.
        vcpu.init_thread_local_data().unwrap();

        // Reset vcpu TLS.
        assert!(vcpu.reset_thread_local_data().is_ok());

        // Second reset should return error.
        assert!(vcpu.reset_thread_local_data().is_err());
    }

    #[test]
    fn test_invalid_tls() {
        let (mut vcpu, _) = setup_vcpu(0x1000);
        // Initialize vcpu TLS.
        vcpu.init_thread_local_data().unwrap();
        // Trying to initialize non-empty TLS should error.
        vcpu.init_thread_local_data().unwrap_err();
    }

    /// Parks `vcpu` in wait_for_event on its own thread and returns it once
    /// woken, so a missed wakeup fails the test instead of hanging it.
    fn spawn_wait_for_event(
        mut vcpu: Vcpu,
        wfe: Receiver<()>,
        timeout: Option<Duration>,
    ) -> Receiver<Vcpu> {
        let (woken_sender, woken) = unbounded();
        thread::spawn(move || {
            vcpu.wait_for_event(0, &wfe, timeout);
            woken_sender.send(vcpu).unwrap();
        });
        woken
    }

    fn expect_woken(woken: &Receiver<Vcpu>) -> Vcpu {
        woken
            .recv_timeout(Duration::from_secs(10))
            .expect("vCPU still parked in wait_for_event")
    }

    #[test]
    fn pause_wakes_vcpu_parked_in_wait_for_event() {
        let (vcpu, _) = setup_vcpu(0x1000);
        let events = vcpu.event_sender.clone().unwrap();
        let (_wfe_sender, wfe) = unbounded();

        let woken = spawn_wait_for_event(vcpu, wfe, None);
        events
            .send(VcpuRequest {
                seq: 1,
                event: VcpuEvent::Pause,
            })
            .unwrap();
        let vcpu = expect_woken(&woken);

        assert!(matches!(
            vcpu.event_receiver.try_recv(),
            Ok(VcpuRequest {
                seq: 1,
                event: VcpuEvent::Pause
            })
        ));
    }

    #[test]
    fn device_irq_wakes_vcpu_parked_in_wait_for_event() {
        let (vcpu, _) = setup_vcpu(0x1000);
        let (wfe_sender, wfe) = unbounded();
        let wfe_after_wake = wfe.clone();

        let woken = spawn_wait_for_event(vcpu, wfe, None);
        wfe_sender.send(()).unwrap();
        let vcpu = expect_woken(&woken);

        assert!(matches!(
            wfe_after_wake.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(matches!(
            vcpu.event_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[test]
    fn device_irq_wakeups_coalesce_while_vcpu_runs() {
        // VcpuList lives in krun-devices, whose tests cannot link HVF.
        let vcpu_list = VcpuList::new(1);
        let wfe = vcpu_list.register(0);

        vcpu_list.wake_vcpu(0);
        vcpu_list.set_irq_common(0, 40);
        vcpu_list.set_irq_common(0, 41);

        assert_eq!(wfe.len(), 1);
    }

    #[test]
    fn vtimer_deadline_wakes_vcpu_parked_in_wait_for_event() {
        let (vcpu, _) = setup_vcpu(0x1000);
        let (_wfe_sender, wfe) = unbounded();

        let woken = spawn_wait_for_event(vcpu, wfe, Some(Duration::from_millis(1)));

        expect_woken(&woken);
    }

    /// A handle wired to channels the test drives in place of a vCPU thread.
    struct FakeVcpu {
        handle: VcpuHandle,
        requests: Receiver<VcpuRequest>,
        reports: Sender<VcpuReport>,
    }

    impl FakeVcpu {
        fn new() -> Self {
            let (event_sender, requests) = unbounded();
            let (reports, report_receiver) = unbounded();
            Self {
                handle: VcpuHandle::new(event_sender, report_receiver, thread::spawn(|| {})),
                requests,
                reports,
            }
        }

        fn reply_to_next_request(&self, response: VcpuResponse) {
            let request = self.requests.try_recv().expect("queued request");
            self.reports
                .send(VcpuReport::Reply {
                    seq: request.seq,
                    response,
                })
                .unwrap();
        }
    }

    #[test]
    fn wait_reply_skips_late_reply_to_abandoned_request() {
        let vcpu = FakeVcpu::new();
        let pause = vcpu.handle.send_event(VcpuEvent::Pause).unwrap();
        assert!(matches!(
            vcpu.handle.wait_reply(pause, Duration::ZERO),
            Err(ReplyError::Timeout)
        ));

        vcpu.reply_to_next_request(VcpuResponse::Paused(vec![1]));
        let resume = vcpu.handle.send_event(VcpuEvent::Resume).unwrap();
        vcpu.reply_to_next_request(VcpuResponse::Resumed);

        assert!(matches!(
            vcpu.handle.wait_reply(resume, Duration::from_secs(10)),
            Ok(VcpuResponse::Resumed)
        ));
        assert!(matches!(
            vcpu.handle.response_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[test]
    fn wait_reply_reports_exit_and_keeps_exit_code() {
        let vcpu = FakeVcpu::new();
        let pause = vcpu.handle.send_event(VcpuEvent::Pause).unwrap();
        vcpu.reports.send(VcpuReport::Exited(3)).unwrap();

        assert!(matches!(
            vcpu.handle.wait_reply(pause, Duration::from_secs(10)),
            Err(ReplyError::Exited(3))
        ));
        assert_eq!(vcpu.handle.exit_code(), Some(3));
    }

    #[test]
    fn exit_code_is_found_behind_stale_replies() {
        let vcpu = FakeVcpu::new();
        let _abandoned = vcpu.handle.send_event(VcpuEvent::Pause).unwrap();
        vcpu.reply_to_next_request(VcpuResponse::Paused(vec![1]));
        vcpu.reports.send(VcpuReport::Exited(4)).unwrap();

        assert_eq!(vcpu.handle.exit_code(), Some(4));
    }

    #[test]
    fn send_event_to_exited_vcpu_is_an_error() {
        let vcpu = FakeVcpu::new();
        drop(vcpu.requests);

        assert!(matches!(
            vcpu.handle.send_event(VcpuEvent::Resume),
            Err(Error::VcpuThreadGone)
        ));
    }

    #[test]
    fn initial_pause_reply_is_matched_by_its_ticket() {
        let (mut vcpu, _) = setup_vcpu(0x1000);
        vcpu.queue_initial_pause();
        let request = vcpu.event_receiver.try_recv().expect("initial pause");
        let reports = vcpu.response_sender.clone();
        let handle = VcpuHandle::new(
            vcpu.event_sender.take().unwrap(),
            vcpu.response_receiver.take().unwrap(),
            thread::spawn(|| {}),
        );
        let first = handle.send_event(VcpuEvent::Resume).unwrap();
        assert_ne!(first.0, request.seq);

        reports
            .send(VcpuReport::Reply {
                seq: request.seq,
                response: VcpuResponse::Paused(vec![]),
            })
            .unwrap();
        assert!(matches!(
            handle.wait_reply(handle.initial_pause_ticket(), Duration::from_secs(10)),
            Ok(VcpuResponse::Paused(_))
        ));
    }
}
