// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Core thread definitions for the runtime scheduler.

use core::mem::offset_of;
use core::ptr;

use cortex_m::{interrupt, peripheral::SCB};

use crate::clock::ticks_per_ms;
use crate::ktimer::{
    CFS_KTIMER, KTimerEntity, dequeue_ktimerq_to_waitq, elapsed_ticks_since_current_reload,
    update_next_ktimer, update_wait_thread_ticks, yield_ktimer,
};
use crate::runq::{SchedEntity, dequeue_runq_to_waitq, enqueue_thread, thread_is_cfs};
use crate::sched::{CURRENT_THREAD_CTX, CURRENT_THREAD_IS_CFS};
use crate::waitq::WaitEntity;

/// Global counter for assigning unique thread IDs. Accessed only
/// from the main thread during thread creation, so no synchronization
/// is needed. When dynamic thread creation is added, this should be
/// protected by a mutex or replaced with an atomic counter.
static mut NEXT_THREAD_ID: u32 = 0;

/// Execution state for a scheduled thread.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    /// The thread is eligible to run when selected by the scheduler.
    Ready,
    /// The thread is currently executing on the CPU.
    Running,
    /// The thread cannot run until an external event or resource becomes ready.
    Waiting,
}

/// 8-byte aligned stack storage for Cortex-M thread contexts.
#[repr(align(8))]
pub struct AlignedStack<const N: usize>(pub [u32; N]);

/// Common scheduler-visible thread context.
///
/// `sp` points at the saved stack frame used when restoring the thread.
/// `exc_return` records whether that saved frame belongs to MSP or PSP and
/// whether an FPU exception frame is active.
#[repr(C)]
pub struct ThreadCtx {
    /// Stack pointer captured for the next restore of this thread.
    /// Stack pointer should be always placed in the first field.
    pub sp: u32,
    /// Saved EXC_RETURN value used to restore the correct stack pointer.
    /// exc_return should be always placed in the second field.
    pub exc_return: u32,
    /// Scheduler-assigned thread identifier.
    pub id: u32,
    /// Human-readable thread name for logs and diagnostics.
    pub name: &'static str,
    /// Current lifecycle state used by the scheduler.
    pub state: ThreadState,
    /// Scheduler class for the concrete thread control block.
    pub is_cfs: bool,
}

impl ThreadCtx {
    /// Return the CFS scheduling entity for this thread, when this is a CFS thread.
    pub fn sched_entity(&self) -> Option<&SchedEntity> {
        if thread_is_cfs(self as *const ThreadCtx) {
            Some(unsafe { &*cfs_sched_entity(self as *const ThreadCtx as *mut ThreadCtx) })
        } else {
            None
        }
    }
}

/// Thread control block for CFS-scheduled threads.
#[repr(C)]
pub struct CfsThread {
    /// Common context. This must remain the first field because assembly and
    /// timer code use `*mut ThreadCtx` as the shared thin pointer type.
    pub thread: ThreadCtx,
    /// Wait entity used for wait-queue ordering.
    pub wait_entity: WaitEntity,
    /// Scheduler entity used for CFS run-queue ordering.
    pub sched_entity: SchedEntity,
}

/// Thread control block for RT-scheduled threads.
#[repr(C)]
pub struct RtThread {
    /// Common context. This must remain the first field because assembly and
    /// timer code use `*mut ThreadCtx` as the shared thin pointer type.
    pub thread: ThreadCtx,
    /// Wait entity used for wait-queue ordering.
    pub wait_entity: WaitEntity,
    ktimer_entity: *mut KTimerEntity,
    pub runtime: u32,
    // deadline miss count
    pub miss_cnt: u32,
}

/// Scheduler-class-specific initialization for concrete thread control blocks.
pub trait ThreadControlBlock {
    const IS_CFS: bool;

    /// Initialize the concrete thread storage and return its common thread pointer.
    ///
    /// # Safety
    ///
    /// `thread` must point to valid writable storage for `Self`.
    unsafe fn init(thread: *mut Self, common: ThreadCtx, priority: u32) -> *mut ThreadCtx;
}

impl ThreadControlBlock for CfsThread {
    const IS_CFS: bool = true;

    unsafe fn init(thread: *mut Self, common: ThreadCtx, priority: u32) -> *mut ThreadCtx {
        assert!(priority != 0, "CFS thread priority must be non-zero");

        unsafe {
            ptr::write(
                thread,
                CfsThread {
                    thread: common,
                    wait_entity: WaitEntity::new(),
                    sched_entity: SchedEntity::new(priority),
                },
            );
            let common_thread = ptr::addr_of_mut!((*thread).thread);
            enqueue_thread(common_thread);
            common_thread
        }
    }
}

impl ThreadControlBlock for RtThread {
    const IS_CFS: bool = false;

    unsafe fn init(thread: *mut Self, common: ThreadCtx, _priority: u32) -> *mut ThreadCtx {
        unsafe {
            ptr::write(
                thread,
                RtThread {
                    thread: common,
                    wait_entity: WaitEntity::new(),
                    ktimer_entity: ptr::null_mut(),
                    runtime: 0,
                    miss_cnt: 0,
                },
            );
            ptr::addr_of_mut!((*thread).thread)
        }
    }
}

pub unsafe fn forkyi<T: ThreadControlBlock>(
    thread: *mut T,
    mut sp: *mut u32,
    entry: extern "C" fn(*mut core::ffi::c_void) -> !,
    arg: *mut core::ffi::c_void,
    name: &'static str,
    priority: u32,
) -> *mut ThreadCtx {
    // Build the initial stack so that, after PendSV restores r4-r11 and sets
    // PSP, exception return consumes a standard hardware frame:
    // r0, r1, r2, r3, r12, lr, pc, xpsr.
    //
    // The initial EXC_RETURN value has bit 4 set, so no floating-point context
    // is restored until the thread actually uses the FPU and hardware records
    // an extended exception frame.
    unsafe {
        // Exception return requires an 8-byte aligned stack.
        sp = ((sp as usize) & !0x7) as *mut u32;

        sp = sp.sub(1);
        *sp = 0x0100_0000; // xPSR: Thumb state

        sp = sp.sub(1);
        *sp = entry as usize as u32; // PC: thread entry point

        sp = sp.sub(1);
        *sp = 0xFFFF_FFFD; // LR: return to Thread mode using PSP

        sp = sp.sub(1);
        *sp = 0x0000_0000; // R12

        for _ in 0..3 {
            sp = sp.sub(1);
            *sp = 0x0000_0000; // R3, R2, R1
        }

        sp = sp.sub(1);
        *sp = arg as u32; // R0: argument to the thread entry function

        for _ in 0..8 {
            sp = sp.sub(1);
            *sp = 0x0000_0000; // R4-R11: initial values
        }
        let id = NEXT_THREAD_ID;
        NEXT_THREAD_ID = NEXT_THREAD_ID.wrapping_add(1);
        let common = ThreadCtx {
            sp: sp as u32,
            exc_return: 0xFFFF_FFFD,
            id,
            name,
            state: ThreadState::Ready,
            is_cfs: T::IS_CFS,
        };
        T::init(thread, common, priority)
    }
}

pub(crate) unsafe fn cfs_sched_entity(thread: *mut ThreadCtx) -> *mut SchedEntity {
    debug_assert!(!thread.is_null());

    let cfs_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, thread))
        .cast::<CfsThread>();

    unsafe { ptr::addr_of_mut!((*cfs_thread).sched_entity) }
}

pub(crate) unsafe fn thread_from_cfs_sched_entity(entity: *mut SchedEntity) -> *mut ThreadCtx {
    debug_assert!(!entity.is_null());

    let cfs_thread = (entity as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, sched_entity))
        .cast::<CfsThread>();

    unsafe { ptr::addr_of_mut!((*cfs_thread).thread) }
}

pub(crate) unsafe fn cfs_wait_entity(thread: *mut ThreadCtx) -> *mut WaitEntity {
    debug_assert!(!thread.is_null());

    let cfs_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, thread))
        .cast::<CfsThread>();

    unsafe { ptr::addr_of_mut!((*cfs_thread).wait_entity) }
}

pub(crate) unsafe fn rt_wait_entity(thread: *mut ThreadCtx) -> *mut WaitEntity {
    debug_assert!(!thread.is_null());

    let rt_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>();

    unsafe { ptr::addr_of_mut!((*rt_thread).wait_entity) }
}

pub(crate) unsafe fn rt_ktimer_entity(thread: *mut ThreadCtx) -> *mut KTimerEntity {
    debug_assert!(!thread.is_null());

    let rt_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>();

    unsafe { (*rt_thread).ktimer_entity }
}

pub(crate) unsafe fn set_rt_ktimer_entity(
    thread: *mut ThreadCtx,
    ktimer_entity: *mut KTimerEntity,
) {
    debug_assert!(!thread.is_null());

    let rt_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>();

    unsafe {
        (*rt_thread).ktimer_entity = ktimer_entity;
    }
}

pub(crate) unsafe fn thread_from_wait_entity(entity: *mut WaitEntity) -> *mut ThreadCtx {
    debug_assert!(!entity.is_null());

    debug_assert_eq!(
        offset_of!(CfsThread, wait_entity),
        offset_of!(RtThread, wait_entity)
    );

    let thread = (entity as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, wait_entity))
        .cast::<CfsThread>();

    unsafe { ptr::addr_of_mut!((*thread).thread) }
}

pub(crate) unsafe fn rt_thread_from_thread_ctx(thread: *mut ThreadCtx) -> *mut RtThread {
    debug_assert!(!thread.is_null());

    (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>()
}

pub fn set_rt_thread_start_time(start_time: u32) -> bool {
    unsafe {
        if !CURRENT_THREAD_IS_CFS {
            let rt_thread = rt_thread_from_thread_ctx(CURRENT_THREAD_CTX);
            (*rt_thread).runtime = start_time;
            true
        } else {
            false
        }
    }
}

pub fn current_rt_thread_runtime() -> Option<u32> {
    interrupt::free(|_| unsafe {
        if CURRENT_THREAD_CTX.is_null() || CURRENT_THREAD_IS_CFS {
            None
        } else {
            let rt_thread = rt_thread_from_thread_ctx(CURRENT_THREAD_CTX);
            Some((*rt_thread).runtime)
        }
    })
}

/// Cooperatively yield the CPU from the running RT thread to the
/// next scheduled left-most timer in the KTimer rbtree.
///
/// This is intended for the threads that have completed their current
/// job and want to give a chance to next scheduled thread.
pub fn yieldyi() {
    interrupt::free(|_| unsafe {
        let elapsed: u32 = elapsed_ticks_since_current_reload();
        let current_ktimer = if CURRENT_THREAD_IS_CFS {
            ptr::addr_of_mut!(CFS_KTIMER.entity)
        } else {
            rt_ktimer_entity(CURRENT_THREAD_CTX)
        };

        let next_ktimer = yield_ktimer(current_ktimer, elapsed, true);
        update_next_ktimer(next_ktimer);
        update_wait_thread_ticks(elapsed);

        SCB::set_pendsv();
    });
}

pub fn msleepyi(msec: u32) {
    interrupt::free(|_| unsafe {
        let elapsed = elapsed_ticks_since_current_reload();
        let current_ktimer = if CURRENT_THREAD_IS_CFS {
            ptr::addr_of_mut!(CFS_KTIMER.entity)
        } else {
            rt_ktimer_entity(CURRENT_THREAD_CTX)
        };

        let next_ktimer = yield_ktimer(current_ktimer, elapsed, false);
        update_next_ktimer(next_ktimer);
        update_wait_thread_ticks(elapsed);

        let wait_entity = if CURRENT_THREAD_IS_CFS {
            cfs_wait_entity(CURRENT_THREAD_CTX)
        } else {
            rt_wait_entity(CURRENT_THREAD_CTX)
        };
        (*wait_entity).wait_ticks = msec.saturating_mul(ticks_per_ms());
        (*wait_entity).waitevt = 0;

        if CURRENT_THREAD_IS_CFS {
            let _ = dequeue_runq_to_waitq(CURRENT_THREAD_CTX);
        } else {
            let _ = dequeue_ktimerq_to_waitq(CURRENT_THREAD_CTX);
        }

        SCB::set_pendsv();
    });
}
