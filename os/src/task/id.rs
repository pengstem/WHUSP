use super::ProcessControlBlock;
use crate::config::{KERNEL_STACK_SIZE, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_STACK_SIZE};
use crate::mm::{KERNEL_SPACE, MapPermission, PhysPageNum, VirtAddr};
use crate::sync::UPIntrFreeCell;
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use lazy_static::*;

pub struct RecycleAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl RecycleAllocator {
    pub fn new() -> Self {
        Self::new_from(0)
    }

    pub fn new_from(current: usize) -> Self {
        RecycleAllocator {
            current,
            recycled: Vec::new(),
        }
    }
    pub fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            id
        } else {
            self.current += 1;
            self.current - 1
        }
    }
    pub fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.contains(&id),
            "id {id} has been deallocated!"
        );
        self.recycled.push(id);
    }

    pub fn dealloc_without_reuse(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.contains(&id),
            "id {id} has been deallocated!"
        );
    }
}

lazy_static! {
    static ref PID_ALLOCATOR: UPIntrFreeCell<RecycleAllocator> =
        unsafe { UPIntrFreeCell::new(RecycleAllocator::new_from(1)) };
    static ref KSTACK_ALLOCATOR: UPIntrFreeCell<RecycleAllocator> =
        unsafe { UPIntrFreeCell::new(RecycleAllocator::new()) };
}

pub const IDLE_PID: usize = 0;

pub struct PidHandle(pub usize);

pub fn pid_alloc() -> PidHandle {
    PidHandle(PID_ALLOCATOR.exclusive_access().alloc())
}

impl Drop for PidHandle {
    fn drop(&mut self) {
        // CONTEXT: Linux avoids immediately reusing a just-reaped PID; LTP
        // fork13 checks this because shell scripts can break when consecutive
        // fork/wait cycles return the same PID. Keep kernel stack and per-process
        // resource IDs recyclable, but let Linux-visible PIDs advance monotonically.
        PID_ALLOCATOR
            .exclusive_access()
            .dealloc_without_reuse(self.0);
    }
}

/// Return (bottom, top) of a kernel stack in kernel space.
pub fn kernel_stack_position(kstack_id: usize) -> (usize, usize) {
    let top = TRAMPOLINE - kstack_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

pub struct KernelStack(pub usize);

pub fn kstack_alloc() -> KernelStack {
    let kstack_id = KSTACK_ALLOCATOR.exclusive_access().alloc();
    let (kstack_bottom, kstack_top) = kernel_stack_position(kstack_id);
    KERNEL_SPACE
        .exclusive_access()
        .insert_kernel_private_framed_area_uninit(
            kstack_bottom.into(),
            kstack_top.into(),
            MapPermission::R | MapPermission::W,
        );
    crate::arch::mm::mark_kernel_tlb_dirty();
    KernelStack(kstack_id)
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let (kernel_stack_bottom, _) = kernel_stack_position(self.0);
        let kernel_stack_bottom_va: VirtAddr = kernel_stack_bottom.into();
        KERNEL_SPACE
            .exclusive_access()
            .remove_area_with_start_vpn(kernel_stack_bottom_va.into());
        crate::arch::mm::mark_kernel_tlb_dirty();
        KSTACK_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

impl KernelStack {
    pub fn get_top(&self) -> usize {
        let (_, kernel_stack_top) = kernel_stack_position(self.0);
        kernel_stack_top
    }

    pub fn bounds(&self) -> (usize, usize) {
        kernel_stack_position(self.0)
    }
}

pub struct TaskUserRes {
    pub tid: usize,
    pub ustack_base: usize,
    pub process: Weak<ProcessControlBlock>,
    user_stack_allocated: bool,
}

const USER_STACK_INITIAL_SIZE: usize = USER_STACK_SIZE;

// TrapContext slots grow downward below the trampoline; the RISC-V restore path
// uses this per-tid virtual address when returning to user mode.
fn trap_cx_bottom_from_tid(tid: usize) -> usize {
    TRAP_CONTEXT_BASE - tid * PAGE_SIZE
}

// Per-thread stack windows keep one unmapped guard page between neighboring
// user stacks by spacing bases with PAGE_SIZE + USER_STACK_SIZE.
fn ustack_bottom_from_tid(ustack_base: usize, tid: usize) -> usize {
    ustack_base + tid * (PAGE_SIZE + USER_STACK_SIZE)
}

fn ustack_mapped_bottom_from_tid(ustack_base: usize, tid: usize) -> usize {
    let ustack_top = ustack_bottom_from_tid(ustack_base, tid) + USER_STACK_SIZE;
    ustack_top - USER_STACK_INITIAL_SIZE.min(USER_STACK_SIZE)
}

impl TaskUserRes {
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let tid = process.inner_exclusive_access().alloc_tid();
        let mut task_user_res = Self {
            tid,
            ustack_base,
            process: Arc::downgrade(&process),
            user_stack_allocated: true,
        };
        if alloc_user_res {
            task_user_res.alloc_user_res();
        }
        task_user_res
    }

    pub fn new_with_supplied_stack(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let tid = process.inner_exclusive_access().alloc_tid();
        let mut task_user_res = Self {
            tid,
            ustack_base,
            process: Arc::downgrade(&process),
            user_stack_allocated: false,
        };
        if alloc_user_res {
            task_user_res.alloc_user_res_without_stack();
        }
        task_user_res
    }

    pub fn alloc_user_res(&mut self) {
        self.alloc_user_res_inner(true);
    }

    pub fn alloc_user_res_without_stack(&mut self) {
        self.alloc_user_res_inner(false);
    }

    fn alloc_user_res_inner(&mut self, allocate_user_stack: bool) {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        self.user_stack_allocated = allocate_user_stack;
        if allocate_user_stack {
            // Reserve the bounded contest stack VMA. Pages are materialized by
            // exec stack setup or by the lazy framed page-fault path.
            let ustack_bottom = ustack_mapped_bottom_from_tid(self.ustack_base, self.tid);
            let ustack_top = ustack_bottom_from_tid(self.ustack_base, self.tid) + USER_STACK_SIZE;
            process_inner.memory_set.insert_lazy_framed_area(
                ustack_bottom.into(),
                ustack_top.into(),
                MapPermission::R | MapPermission::W | MapPermission::U,
            );
        }
        // Map one kernel-private TrapContext page in the process page table.
        // It must stay non-user-accessible; trap return code reaches it through
        // the per-task PPN or fixed per-tid virtual address.
        let trap_cx_bottom = trap_cx_bottom_from_tid(self.tid);
        let trap_cx_top = trap_cx_bottom + PAGE_SIZE;
        process_inner
            .memory_set
            .insert_kernel_private_framed_area_uninit(
                trap_cx_bottom.into(),
                trap_cx_top.into(),
                MapPermission::R | MapPermission::W,
            );
    }

    fn dealloc_user_res(&self) {
        // The process-local task slot id is recyclable independently from the
        // Linux-visible TID/PID handles used by futex, signal, and procfs code.
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        // Remove only the default contest stack VMA owned by TaskUserRes.
        // Pthread-supplied stacks belong to userspace mappings.
        if self.user_stack_allocated {
            let ustack_bottom_va: VirtAddr =
                ustack_mapped_bottom_from_tid(self.ustack_base, self.tid).into();
            process_inner
                .memory_set
                .remove_area_with_start_vpn(ustack_bottom_va.into());
        }
        // The TrapContext mapping is always owned by TaskUserRes, including
        // pthreads that supplied their own userspace stack.
        let trap_cx_bottom_va: VirtAddr = trap_cx_bottom_from_tid(self.tid).into();
        process_inner
            .memory_set
            .remove_area_with_start_vpn(trap_cx_bottom_va.into());
    }

    pub fn dealloc_tid(&self) {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        process_inner.dealloc_tid(self.tid);
    }

    #[cfg(target_arch = "riscv64")]
    pub fn trap_cx_user_va(&self) -> usize {
        trap_cx_bottom_from_tid(self.tid)
    }

    pub fn trap_cx_ppn(&self) -> PhysPageNum {
        let process = self.process.upgrade().unwrap();
        let process_inner = process.inner_exclusive_access();
        let trap_cx_bottom_va: VirtAddr = trap_cx_bottom_from_tid(self.tid).into();
        process_inner
            .memory_set
            .translate(trap_cx_bottom_va.into())
            .unwrap()
            .ppn()
    }

    pub fn ustack_base(&self) -> usize {
        self.ustack_base
    }
    pub fn ustack_top(&self) -> usize {
        ustack_bottom_from_tid(self.ustack_base, self.tid) + USER_STACK_SIZE
    }
}

impl Drop for TaskUserRes {
    fn drop(&mut self) {
        self.dealloc_tid();
        self.dealloc_user_res();
    }
}
