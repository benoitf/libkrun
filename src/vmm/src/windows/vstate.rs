// Copyright 2024 The libkrun Authors. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// WHP (Windows Hypervisor Platform) backend for libkrun.

use std::cell::Cell;
use std::fmt::{Display, Formatter};
use std::io;
use std::result;
use std::sync::Arc;
use std::thread;

use super::super::{FC_EXIT_CODE_GENERIC_ERROR, FC_EXIT_CODE_OK};
use crate::vmm_config::machine_config::CpuFeaturesTemplate;

use arch::ArchMemoryInfo;
use crossbeam_channel::{unbounded, Receiver, Sender};
use utils::eventfd::EventFd;
use vm_memory::{
    Address, GuestAddress, GuestMemory, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion,
};

use windows::Win32::System::Hypervisor::*;

/// Construct a GDT entry from flags, base, and limit (matches arch::x86_64::gdt::gdt_entry).
#[cfg(target_arch = "x86_64")]
fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000u64) << (56 - 24))
        | ((u64::from(flags) & 0x0000_f0ffu64) << 40)
        | ((u64::from(limit) & 0x000f_0000u64) << (48 - 16))
        | ((u64::from(base) & 0x00ff_ffffu64) << 16)
        | (u64::from(limit) & 0x0000_ffffu64)
}

/// Build a WHP segment register from a GDT entry.
/// The Attributes field uses the x86 segment access rights format:
///   bits 0-3: type, bit 4: S, bits 5-6: DPL, bit 7: P,
///   bit 8: AVL, bit 9: L, bit 10: D/B, bit 11: G
#[cfg(target_arch = "x86_64")]
fn whp_segment_from_gdt(entry: u64, table_index: u8) -> WHV_X64_SEGMENT_REGISTER {
    let seg_type = ((entry & 0x0000_0F00_0000_0000) >> 40) as u16;
    let s = ((entry & 0x0000_1000_0000_0000) >> 44) as u16;
    let dpl = ((entry & 0x0000_6000_0000_0000) >> 45) as u16;
    let present = ((entry & 0x0000_8000_0000_0000) >> 47) as u16;
    let avl = ((entry & 0x0010_0000_0000_0000) >> 52) as u16;
    let l = ((entry & 0x0020_0000_0000_0000) >> 53) as u16;
    let db = ((entry & 0x0040_0000_0000_0000) >> 54) as u16;
    let g = ((entry & 0x0080_0000_0000_0000) >> 55) as u16;

    let attributes: u16 = seg_type
        | (s << 4)
        | (dpl << 5)
        | (present << 7)
        | (avl << 8)
        | (l << 9)
        | (db << 10)
        | (g << 11);

    let base = (((entry) & 0xFF00_0000_0000_0000) >> 32)
        | (((entry) & 0x0000_00FF_0000_0000) >> 16)
        | (((entry) & 0x0000_0000_FFFF_0000) >> 16);

    let limit = ((((entry) & 0x000F_0000_0000_0000) >> 32) | ((entry) & 0x0000_0000_0000_FFFF)) as u32;

    WHV_X64_SEGMENT_REGISTER {
        Base: base,
        Limit: limit,
        Selector: (table_index as u16) * 8,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: attributes,
        },
    }
}

/// Handle wrapper for Send/Sync safety (WHP partition handle).
#[derive(Debug)]
struct PartitionHandle(WHV_PARTITION_HANDLE);

// SAFETY: WHP partition handles can be used from any thread.
unsafe impl Send for PartitionHandle {}
unsafe impl Sync for PartitionHandle {}

/// Errors associated with WHP operations.
#[derive(Debug)]
pub enum Error {
    /// Invalid guest memory configuration.
    GuestMemoryMmap(GuestMemoryError),
    /// The number of configured slots is bigger than the maximum reported.
    NotEnoughMemorySlots,
    /// Cannot set the memory regions.
    SetUserMemoryRegion(windows::core::Error),
    /// Failed to signal Vcpu.
    SignalVcpu(utils::errno::Error),
    /// vCPU count is not initialized.
    VcpuCountNotInitialized,
    /// Cannot run the VCPUs.
    VcpuRun,
    /// Cannot spawn a new vCPU thread.
    VcpuSpawn(io::Error),
    /// Cannot cleanly initialize vcpu TLS.
    VcpuTlsInit,
    /// Vcpu not present in TLS.
    VcpuTlsNotPresent,
    /// Unexpected exit reason.
    VcpuUnhandledKvmExit,
    /// Cannot configure the microvm.
    VmSetup(windows::core::Error),
    /// WHP API error.
    WhpError(windows::core::Error),
    /// Error configuring registers.
    #[cfg(target_arch = "x86_64")]
    REGSConfiguration(windows::core::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            GuestMemoryMmap(e) => write!(f, "Guest memory error: {e:?}"),
            VcpuCountNotInitialized => write!(f, "vCPU count is not initialized"),
            VmSetup(e) => write!(f, "Cannot configure the microvm: {e}"),
            VcpuRun => write!(f, "Cannot run the VCPUs"),
            NotEnoughMemorySlots => write!(f, "Not enough memory slots"),
            SetUserMemoryRegion(e) => write!(f, "Cannot set the memory regions: {e}"),
            SignalVcpu(e) => write!(f, "Failed to signal Vcpu: {e}"),
            VcpuSpawn(e) => write!(f, "Cannot spawn a new vCPU thread: {e}"),
            VcpuTlsInit => write!(f, "Cannot clean init vcpu TLS"),
            VcpuTlsNotPresent => write!(f, "Vcpu not present in TLS"),
            VcpuUnhandledKvmExit => write!(f, "Unexpected exit reason"),
            WhpError(e) => write!(f, "WHP error: {e}"),
            #[cfg(target_arch = "x86_64")]
            REGSConfiguration(e) => write!(f, "Error configuring registers: {e}"),
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

/// A wrapper around a WHP partition (VM).
pub struct Vm {
    partition: PartitionHandle,
}

impl Vm {
    /// Creates a new WHP partition.
    pub fn new(_nested_enabled: bool) -> Result<Self> {
        let partition =
            unsafe { WHvCreatePartition() }.map_err(Error::VmSetup)?;

        // Set processor count - will be updated before setup
        let property = WHV_PARTITION_PROPERTY {
            ProcessorCount: 1,
        };
        unsafe {
            WHvSetPartitionProperty(
                partition,
                WHvPartitionPropertyCodeProcessorCount,
                &property as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )
        }
        .map_err(Error::VmSetup)?;

        unsafe { WHvSetupPartition(partition) }.map_err(Error::VmSetup)?;

        Ok(Vm {
            partition: PartitionHandle(partition),
        })
    }

    /// Returns the partition handle.
    pub fn partition_handle(&self) -> WHV_PARTITION_HANDLE {
        self.partition.0
    }

    /// Initializes guest memory by mapping regions into the partition.
    pub fn memory_init(&mut self, guest_mem: &GuestMemoryMmap) -> Result<()> {
        for region in guest_mem.iter() {
            let host_addr = guest_mem.get_host_address(region.start_addr()).unwrap();
            debug!(
                "Guest memory host_addr={:x?} guest_addr={:x?} len={:x?}",
                host_addr,
                region.start_addr().raw_value(),
                region.len()
            );
            unsafe {
                WHvMapGpaRange(
                    self.partition.0,
                    host_addr as *const std::ffi::c_void,
                    region.start_addr().raw_value(),
                    region.len(),
                    WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
                )
            }
            .map_err(Error::SetUserMemoryRegion)?;
        }

        Ok(())
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = unsafe { WHvDeletePartition(self.partition.0) };
    }
}

/// Encapsulates configuration parameters for the guest vCPUs.
#[derive(Debug, Eq, PartialEq)]
pub struct VcpuConfig {
    /// Number of guest VCPUs.
    pub vcpu_count: u8,
    /// Enable hyperthreading in the CPUID configuration.
    pub ht_enabled: bool,
    /// CPUID template to use.
    pub cpu_template: Option<CpuFeaturesTemplate>,
}

type VcpuCell = Cell<Option<*const Vcpu>>;

/// A wrapper around a WHP virtual processor.
pub struct Vcpu {
    id: u8,
    partition: WHV_PARTITION_HANDLE,
    mmio_bus: Option<devices::Bus>,
    pio_bus: Option<devices::Bus>,
    exit_evt: EventFd,

    #[allow(unused)]
    event_receiver: Receiver<VcpuEvent>,
    event_sender: Option<Sender<VcpuEvent>>,
    response_receiver: Option<Receiver<VcpuResponse>>,
    response_sender: Sender<VcpuResponse>,
}

// SAFETY: The WHP partition handle is safe to use across threads.
unsafe impl Send for Vcpu {}

impl Vcpu {
    thread_local!(static TLS_VCPU_PTR: VcpuCell = const { Cell::new(None) });

    fn init_thread_local_data(&mut self) -> Result<()> {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if cell.get().is_some() {
                return Err(Error::VcpuTlsInit);
            }
            cell.set(Some(self as *const Vcpu));
            Ok(())
        })
    }

    fn reset_thread_local_data(&mut self) -> Result<()> {
        Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| {
            if let Some(vcpu_ptr) = cell.get() {
                if std::ptr::eq(vcpu_ptr, self) {
                    Self::TLS_VCPU_PTR.with(|cell: &VcpuCell| cell.take());
                    return Ok(());
                }
            }
            Err(Error::VcpuTlsNotPresent)
        })
    }

    /// No-op on Windows; we use WHvCancelRunVirtualProcessor instead of signals.
    pub fn register_kick_signal_handler() {}

    /// Creates a new WHP virtual processor.
    #[cfg(target_arch = "x86_64")]
    pub fn new_x86_64(
        id: u8,
        partition: WHV_PARTITION_HANDLE,
        pio_bus: devices::Bus,
        exit_evt: EventFd,
    ) -> Result<Self> {
        unsafe { WHvCreateVirtualProcessor(partition, id as u32, 0) }
            .map_err(Error::WhpError)?;

        let (event_sender, event_receiver) = unbounded();
        let (response_sender, response_receiver) = unbounded();

        Ok(Vcpu {
            id,
            partition,
            mmio_bus: None,
            pio_bus: Some(pio_bus),
            exit_evt,
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
        })
    }

    /// Returns the cpu index as seen by the guest OS.
    pub fn cpu_index(&self) -> u8 {
        self.id
    }

    /// Sets a MMIO bus for this vcpu.
    pub fn set_mmio_bus(&mut self, mmio_bus: devices::Bus) {
        self.mmio_bus = Some(mmio_bus);
    }

    #[cfg(target_arch = "x86_64")]
    pub fn configure_x86_64(
        &mut self,
        guest_mem: &GuestMemoryMmap,
        kernel_start_addr: GuestAddress,
        vcpu_config: &VcpuConfig,
    ) -> Result<()> {
        let _ = vcpu_config;

        // Only set up page tables and memory structures for the BSP (id == 0).
        if self.id == 0 {
            Self::setup_page_tables(guest_mem)?;
            Self::setup_gdt_idt(guest_mem)?;
        }

        self.setup_sregs()?;
        self.setup_regs(kernel_start_addr.raw_value())?;
        self.setup_fpu()?;
        self.setup_msrs()?;

        Ok(())
    }

    /// Write page tables into guest memory: identity-map the first 1GB using 2MB pages.
    #[cfg(target_arch = "x86_64")]
    fn setup_page_tables(guest_mem: &GuestMemoryMmap) -> Result<()> {
        use vm_memory::Bytes;

        const PML4_START: u64 = 0x9000;
        const PDPTE_START: u64 = 0xa000;
        const PDE_START: u64 = 0xb000;

        // PML4[0] -> PDPTE
        guest_mem
            .write_obj(PDPTE_START | 0x03u64, GuestAddress(PML4_START))
            .map_err(|_| Error::WhpError(windows::core::Error::empty()))?;

        // PDPTE[0] -> PDE
        guest_mem
            .write_obj(PDE_START | 0x03u64, GuestAddress(PDPTE_START))
            .map_err(|_| Error::WhpError(windows::core::Error::empty()))?;

        // 512 x 2MB pages covering [0..1GB)
        for i in 0u64..512 {
            guest_mem
                .write_obj(
                    (i << 21) | 0x83u64,
                    GuestAddress(PDE_START + i * 8),
                )
                .map_err(|_| Error::WhpError(windows::core::Error::empty()))?;
        }

        Ok(())
    }

    /// Write GDT and IDT into guest memory.
    #[cfg(target_arch = "x86_64")]
    fn setup_gdt_idt(guest_mem: &GuestMemoryMmap) -> Result<()> {
        use vm_memory::Bytes;

        const BOOT_GDT_OFFSET: u64 = 0x500;
        const BOOT_IDT_OFFSET: u64 = 0x520;

        // GDT entries: NULL, CODE (0xa09b), DATA (0xc093), TSS (0x808b)
        let gdt_table: [u64; 4] = [
            gdt_entry(0, 0, 0),
            gdt_entry(0xa09b, 0, 0xfffff),
            gdt_entry(0xc093, 0, 0xfffff),
            gdt_entry(0x808b, 0, 0xfffff),
        ];

        for (i, entry) in gdt_table.iter().enumerate() {
            guest_mem
                .write_obj(
                    *entry,
                    GuestAddress(BOOT_GDT_OFFSET + (i as u64) * 8),
                )
                .map_err(|_| Error::WhpError(windows::core::Error::empty()))?;
        }

        // IDT: zeroed
        guest_mem
            .write_obj(0u64, GuestAddress(BOOT_IDT_OFFSET))
            .map_err(|_| Error::WhpError(windows::core::Error::empty()))?;

        Ok(())
    }

    /// Set segment and control registers via WHP.
    #[cfg(target_arch = "x86_64")]
    fn setup_sregs(&self) -> Result<()> {
        const BOOT_GDT_OFFSET: u64 = 0x500;
        const BOOT_IDT_OFFSET: u64 = 0x520;
        const PML4_START: u64 = 0x9000;

        const X86_CR0_PE: u64 = 0x1;
        const X86_CR0_PG: u64 = 0x8000_0000;
        const X86_CR4_PAE: u64 = 0x20;
        const EFER_LME: u64 = 0x100;
        const EFER_LMA: u64 = 0x400;

        // Build GDT entries to extract segment attributes
        let code_gdt = gdt_entry(0xa09b, 0, 0xfffff);
        let data_gdt = gdt_entry(0xc093, 0, 0xfffff);
        let tss_gdt = gdt_entry(0x808b, 0, 0xfffff);

        let code_seg = whp_segment_from_gdt(code_gdt, 1);
        let data_seg = whp_segment_from_gdt(data_gdt, 2);
        let tss_seg = whp_segment_from_gdt(tss_gdt, 3);

        let reg_names = [
            WHvX64RegisterCs,
            WHvX64RegisterDs,
            WHvX64RegisterEs,
            WHvX64RegisterFs,
            WHvX64RegisterGs,
            WHvX64RegisterSs,
            WHvX64RegisterTr,
            WHvX64RegisterGdtr,
            WHvX64RegisterIdtr,
            WHvX64RegisterCr0,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterEfer,
        ];

        let gdtr = WHV_X64_TABLE_REGISTER {
            Pad: [0; 3],
            Limit: (4 * 8 - 1) as u16, // 4 GDT entries
            Base: BOOT_GDT_OFFSET,
        };

        let idtr = WHV_X64_TABLE_REGISTER {
            Pad: [0; 3],
            Limit: 7, // size_of::<u64>() - 1
            Base: BOOT_IDT_OFFSET,
        };

        let reg_values = [
            WHV_REGISTER_VALUE { Segment: code_seg },  // CS
            WHV_REGISTER_VALUE { Segment: data_seg },  // DS
            WHV_REGISTER_VALUE { Segment: data_seg },  // ES
            WHV_REGISTER_VALUE { Segment: data_seg },  // FS
            WHV_REGISTER_VALUE { Segment: data_seg },  // GS
            WHV_REGISTER_VALUE { Segment: data_seg },  // SS
            WHV_REGISTER_VALUE { Segment: tss_seg },   // TR
            WHV_REGISTER_VALUE { Table: gdtr },         // GDTR
            WHV_REGISTER_VALUE { Table: idtr },         // IDTR
            WHV_REGISTER_VALUE { Reg64: X86_CR0_PE | X86_CR0_PG }, // CR0
            WHV_REGISTER_VALUE { Reg64: PML4_START },   // CR3
            WHV_REGISTER_VALUE { Reg64: X86_CR4_PAE },  // CR4
            WHV_REGISTER_VALUE { Reg64: EFER_LME | EFER_LMA }, // EFER
        ];

        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition,
                self.id as u32,
                reg_names.as_ptr(),
                reg_names.len() as u32,
                reg_values.as_ptr(),
            )
        }
        .map_err(Error::REGSConfiguration)?;

        Ok(())
    }

    /// Set general-purpose registers via WHP.
    #[cfg(target_arch = "x86_64")]
    fn setup_regs(&self, boot_ip: u64) -> Result<()> {
        const BOOT_STACK_POINTER: u64 = 0x8ff0;
        const ZERO_PAGE_START: u64 = 0x7000;

        let reg_names = [
            WHvX64RegisterRip,
            WHvX64RegisterRsp,
            WHvX64RegisterRbp,
            WHvX64RegisterRsi,
            WHvX64RegisterRflags,
        ];

        let reg_values = [
            WHV_REGISTER_VALUE { Reg64: boot_ip },
            WHV_REGISTER_VALUE { Reg64: BOOT_STACK_POINTER },
            WHV_REGISTER_VALUE { Reg64: BOOT_STACK_POINTER },
            WHV_REGISTER_VALUE { Reg64: ZERO_PAGE_START },
            WHV_REGISTER_VALUE { Reg64: 0x0000_0000_0000_0002u64 },
        ];

        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition,
                self.id as u32,
                reg_names.as_ptr(),
                reg_names.len() as u32,
                reg_values.as_ptr(),
            )
        }
        .map_err(Error::REGSConfiguration)?;

        Ok(())
    }

    /// Set FPU state via WHP.
    #[cfg(target_arch = "x86_64")]
    fn setup_fpu(&self) -> Result<()> {
        let mut fp_control = WHV_X64_FP_CONTROL_STATUS_REGISTER::default();
        unsafe {
            fp_control.Anonymous.FpControl = 0x37f;
        }

        let mut xmm_control = WHV_X64_XMM_CONTROL_STATUS_REGISTER::default();
        unsafe {
            xmm_control.Anonymous.XmmStatusControl = 0x1f80;
        }

        let reg_names = [
            WHvX64RegisterFpControlStatus,
            WHvX64RegisterXmmControlStatus,
        ];

        let reg_values = [
            WHV_REGISTER_VALUE {
                FpControlStatus: fp_control,
            },
            WHV_REGISTER_VALUE {
                XmmControlStatus: xmm_control,
            },
        ];

        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition,
                self.id as u32,
                reg_names.as_ptr(),
                reg_names.len() as u32,
                reg_values.as_ptr(),
            )
        }
        .map_err(Error::REGSConfiguration)?;

        Ok(())
    }

    /// Set MSRs via WHP.
    #[cfg(target_arch = "x86_64")]
    fn setup_msrs(&self) -> Result<()> {
        // IA32_MISC_ENABLE: fast string enable
        const MSR_IA32_MISC_ENABLE_FAST_STRING: u64 = 0x1;
        // MTRRdefType: enable MTRRs + default type WB (write-back = 6)
        const MTRR_ENABLE: u64 = 0x800;
        const MTRR_WB: u64 = 0x6;

        let reg_names = [
            WHvX64RegisterTsc,
            WHvX64RegisterSysenterCs,
            WHvX64RegisterSysenterEsp,
            WHvX64RegisterSysenterEip,
            WHvX64RegisterStar,
            WHvX64RegisterLstar,
            WHvX64RegisterCstar,
            WHvX64RegisterSfmask,
            WHvX64RegisterKernelGsBase,
            WHvX64RegisterMsrMtrrDefType,
        ];

        let reg_values = [
            WHV_REGISTER_VALUE { Reg64: 0 },                           // TSC
            WHV_REGISTER_VALUE { Reg64: 0 },                           // SYSENTER_CS
            WHV_REGISTER_VALUE { Reg64: 0 },                           // SYSENTER_ESP
            WHV_REGISTER_VALUE { Reg64: 0 },                           // SYSENTER_EIP
            WHV_REGISTER_VALUE { Reg64: 0 },                           // STAR
            WHV_REGISTER_VALUE { Reg64: 0 },                           // LSTAR
            WHV_REGISTER_VALUE { Reg64: 0 },                           // CSTAR
            WHV_REGISTER_VALUE { Reg64: 0 },                           // SFMASK
            WHV_REGISTER_VALUE { Reg64: 0 },                           // KERNEL_GS_BASE
            WHV_REGISTER_VALUE { Reg64: MTRR_ENABLE | MTRR_WB },       // MTRRdefType
        ];

        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition,
                self.id as u32,
                reg_names.as_ptr(),
                reg_names.len() as u32,
                reg_values.as_ptr(),
            )
        }
        .map_err(Error::REGSConfiguration)?;

        Ok(())
    }

    /// Moves the vcpu to its own thread and constructs a VcpuHandle.
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

    /// Main loop of the vCPU thread.
    pub fn run(&mut self, init_tls_sender: Sender<bool>) {
        init_tls_sender
            .send(true)
            .expect("Cannot notify vcpu TLS initialization.");

        loop {
            match self.run_emulation() {
                Ok(VcpuEmulation::Handled) => (),
                Ok(VcpuEmulation::Stopped) => {
                    self.exit(FC_EXIT_CODE_OK);
                    break;
                }
                Err(_) => {
                    self.exit(FC_EXIT_CODE_GENERIC_ERROR);
                    break;
                }
            }
        }
    }

    fn run_emulation(&mut self) -> Result<VcpuEmulation> {
        let mut exit_context: WHV_RUN_VP_EXIT_CONTEXT = unsafe { std::mem::zeroed() };

        let result = unsafe {
            WHvRunVirtualProcessor(
                self.partition,
                self.id as u32,
                &mut exit_context as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };

        if let Err(e) = result {
            error!("WHvRunVirtualProcessor failed: {e}");
            return Err(Error::VcpuRun);
        }

        match exit_context.ExitReason {
            WHvRunVpExitReasonMemoryAccess => {
                let mem_access = unsafe { exit_context.Anonymous.MemoryAccess };
                let gpa = mem_access.Gpa;
                let access_info = unsafe { mem_access.AccessInfo.Anonymous._bitfield };
                let is_write = (access_info & 0x1) != 0;
                let data_len = mem_access.InstructionByteCount as usize;
                let mut data_buf = mem_access.InstructionBytes[..data_len].to_vec();
                let data_slice = &mut data_buf[..];

                if let Some(ref mmio_bus) = self.mmio_bus {
                    if is_write {
                        mmio_bus.write(self.id as u64, gpa, data_slice);
                    } else {
                        mmio_bus.read(self.id as u64, gpa, data_slice);
                    }
                }

                // Advance RIP past the instruction - WHP does NOT auto-increment RIP
                self.advance_rip(&exit_context)?;

                Ok(VcpuEmulation::Handled)
            }
            WHvRunVpExitReasonX64IoPortAccess => {
                let io_access = unsafe { exit_context.Anonymous.IoPortAccess };
                let port = io_access.PortNumber;
                let access_bitfield = unsafe { io_access.AccessInfo.Anonymous._bitfield };
                let is_write = (access_bitfield & 0x1) != 0;
                let access_size = ((access_bitfield >> 16) & 0x7) as usize;

                if let Some(ref pio_bus) = self.pio_bus {
                    if is_write {
                        let data = io_access.Rax.to_le_bytes();
                        pio_bus.write(self.id as u64, port as u64, &data[..access_size]);
                    } else {
                        let mut data = [0u8; 8];
                        pio_bus.read(self.id as u64, port as u64, &mut data[..access_size]);
                    }
                }

                // Advance RIP - WHP does NOT auto-increment RIP for IO port intercepts
                self.advance_rip(&exit_context)?;

                Ok(VcpuEmulation::Handled)
            }
            WHvRunVpExitReasonX64Halt => {
                info!("vCPU {} halted", self.id);
                Ok(VcpuEmulation::Stopped)
            }
            WHvRunVpExitReasonCanceled => {
                debug!("vCPU {} canceled", self.id);
                Ok(VcpuEmulation::Handled)
            }
            WHvRunVpExitReasonNone => Ok(VcpuEmulation::Handled),
            other => {
                error!("Unhandled WHP exit reason: {:?}", other);
                Err(Error::VcpuUnhandledKvmExit)
            }
        }
    }

    /// Advance RIP past the current instruction.
    /// WHP does not auto-increment RIP after IO/MMIO intercepts.
    fn advance_rip(&self, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
        // Instruction length is in the lower 4 bits of the bitfield
        let insn_len = unsafe { exit_context.VpContext.ExecutionState.Anonymous._bitfield } & 0xF;
        if insn_len == 0 {
            return Ok(());
        }

        let rip = unsafe { exit_context.VpContext.Rip } + insn_len as u64;

        let reg_name = WHV_REGISTER_NAME(WHvX64RegisterRip.0);
        let reg_value = WHV_REGISTER_VALUE {
            Reg64: rip,
        };

        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition,
                self.id as u32,
                &reg_name,
                1,
                &reg_value,
            )
        }
        .map_err(Error::WhpError)?;

        Ok(())
    }

    fn exit(&mut self, exit_code: u8) {
        self.response_sender
            .send(VcpuResponse::Exited(exit_code))
            .expect("failed to send Exited status");

        if let Err(e) = self.exit_evt.write(1) {
            error!("Failed signaling vcpu exit event: {e}");
        }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        let _ = unsafe { WHvDeleteVirtualProcessor(self.partition, self.id as u32) };
        let _ = self.reset_thread_local_data();
    }
}

#[allow(unused)]
#[derive(Debug)]
pub enum VcpuEvent {
    Pause,
    Resume,
}

#[derive(Debug, Eq, PartialEq)]
pub enum VcpuResponse {
    Paused,
    Resumed,
    Exited(u8),
}

/// Wrapper over Vcpu that hides the underlying interactions with the Vcpu thread.
pub struct VcpuHandle {
    event_sender: Sender<VcpuEvent>,
    response_receiver: Receiver<VcpuResponse>,
}

impl VcpuHandle {
    pub fn new(
        event_sender: Sender<VcpuEvent>,
        response_receiver: Receiver<VcpuResponse>,
        _vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            event_sender,
            response_receiver,
        }
    }

    pub fn send_event(&self, event: VcpuEvent) -> Result<()> {
        self.event_sender
            .send(event)
            .expect("event sender channel closed on vcpu end.");
        Ok(())
    }

    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
    }
}

enum VcpuEmulation {
    Handled,
    Stopped,
}
