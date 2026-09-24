//! Global Descriptor Table + Task State Segment.
//!
//! We only need this so the double-fault handler can run on its own
//! private stack (IST) — otherwise a stack-overflow-triggered double
//! fault would fault again trying to push onto the same broken stack and
//! triple-fault the machine instead of giving us a diagnostic.

use lazy_static::lazy_static;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, FS, GS, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            let stack_start = VirtAddr::from_ptr(core::ptr::addr_of!(STACK));
            stack_start + STACK_SIZE as u64
        };
        // RSP0: the stack the CPU *automatically* switches to on any
        // interrupt/exception/`int 0x80` that lands while running ring 3
        // (any privilege-level-changing transition), before it pushes
        // even the first byte of the interrupt frame. Without this set,
        // that switch loads an undefined stack pointer and the very
        // first such interrupt triple-faults the machine — this must
        // exist before `task::thread` ever schedules a ring-3 thread.
        tss.privilege_stack_table[0] = {
            const STACK_SIZE: usize = 4096 * 4;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            let stack_start = VirtAddr::from_ptr(core::ptr::addr_of!(STACK));
            stack_start + STACK_SIZE as u64
        };
        tss
    };
}

struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        let data_selector = gdt.append(Descriptor::kernel_data_segment());
        let user_code_selector = gdt.append(Descriptor::user_code_segment());
        let user_data_selector = gdt.append(Descriptor::user_data_segment());
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));
        (
            gdt,
            Selectors {
                code_selector,
                data_selector,
                user_code_selector,
                user_data_selector,
                tss_selector,
            },
        )
    };
}

/// The kernel code/data selectors, for anything that needs to hand-build
/// a CPU interrupt frame (see `task::thread`, which fabricates one for a
/// new kernel thread's very first `iretq`-into-existence).
pub fn kernel_code_selector() -> SegmentSelector {
    GDT.1.code_selector
}

pub fn kernel_data_selector() -> SegmentSelector {
    GDT.1.data_selector
}

/// The ring-3 (DPL 3) code/data selectors — `task::thread`'s user-mode
/// threads `iretq` into these instead of the kernel ones above.
pub fn user_code_selector() -> SegmentSelector {
    GDT.1.user_code_selector
}

pub fn user_data_selector() -> SegmentSelector {
    GDT.1.user_data_selector
}

pub fn init() {
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.code_selector);
        // The bootloader's own GDT (now gone, replaced by ours above) had
        // left SS/DS/ES/FS/GS pointing at whatever selector index it
        // used for its data segment. That index means something
        // different in *our* GDT — here, it lands on the first half of
        // the (system-type) TSS descriptor. A stale SS like that is
        // invalid to restore on `iretq` (it must reference a proper data
        // segment), which faults with no handler registered for it and
        // escalates straight to a double fault. Point every segment
        // register at our real data segment (or null, where a selector
        // isn't load-bearing) before anything can interrupt us.
        SS::set_reg(GDT.1.data_selector);
        DS::set_reg(GDT.1.data_selector);
        ES::set_reg(GDT.1.data_selector);
        FS::set_reg(SegmentSelector::NULL);
        GS::set_reg(SegmentSelector::NULL);
        load_tss(GDT.1.tss_selector);
    }
}
