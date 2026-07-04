// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Core thread definitions for the runtime scheduler.

use core::mem::offset_of;
use core::ptr;

use crate::arch::ctx_swtich::request_context_switch;
use crate::clock::ticks_per_ms;
use crate::critical_section;
use crate::ktimer::{
    CFS_KTIMER, KTimerEntity, RtKTimer, dequeue_ktimerq_to_waitq,
    elapsed_ticks_since_current_reload, enqueue_ktimer, ktimer_now_ticks, update_next_ktimer,
    yield_ktimer,
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

    /// Return this thread's remaining wait ticks and wait event.
    pub fn wait_info(&self) -> (u32, u32) {
        unsafe {
            let thread = self as *const ThreadCtx as *mut ThreadCtx;
            let entity = if self.is_cfs {
                cfs_wait_entity(thread)
            } else {
                rt_wait_entity(thread)
            };

            (
                (*entity).remaining_at(ktimer_now_ticks()),
                (*entity).waitevt,
            )
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
    /// KTimer entity used for KTimerQueue ordering.
    pub ktimer_entity: *mut KTimerEntity,
    /// Elapsed tick counter for the RT thread's current period/job window.
    ///
    /// This is not only CPU execution time: it also includes time spent waiting
    /// after the thread yields into the wait queue, so RT deadlines continue to
    /// advance while the thread is sleeping. The counter is charged when the RT
    /// thread yields or is preempted, and also when it wakes from the wait queue.
    /// It is reset when the RT thread starts a new period.
    pub runtime: u32,
}

/// Scheduler-class-specific initialization for concrete thread control blocks.
pub trait ThreadControlBlock {
    const IS_CFS: bool;

    /// Scheduler-class-specific initialization argument.
    type InitArgs;

    /// Initialize the concrete thread storage and return its common thread pointer.
    ///
    /// # Safety
    ///
    /// `thread` must point to valid writable storage for `Self`.
    unsafe fn init(thread: *mut Self, common: ThreadCtx, args: Self::InitArgs) -> *mut ThreadCtx;
}

impl ThreadControlBlock for CfsThread {
    const IS_CFS: bool = true;
    type InitArgs = u32;

    unsafe fn init(
        thread: *mut Self,
        common: ThreadCtx,
        priority: Self::InitArgs,
    ) -> *mut ThreadCtx {
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
    type InitArgs = *mut RtKTimer;

    unsafe fn init(thread: *mut Self, common: ThreadCtx, ktimer: Self::InitArgs) -> *mut ThreadCtx {
        assert!(!ktimer.is_null(), "RT thread ktimer must be non-null");

        unsafe {
            ptr::write(
                thread,
                RtThread {
                    thread: common,
                    wait_entity: WaitEntity::new(),
                    ktimer_entity: ptr::null_mut(),
                    runtime: 0,
                },
            );
            let common_thread = ptr::addr_of_mut!((*thread).thread);
            (*ktimer).init_rt_ktimer(common_thread);
            enqueue_ktimer((*ktimer).entity_mut());
            common_thread
        }
    }
}

pub unsafe fn forkyi<T: ThreadControlBlock>(
    thread: *mut T,
    mut sp: *mut u32,
    entry: extern "C" fn(*mut core::ffi::c_void) -> !,
    arg: *mut core::ffi::c_void,
    name: &'static str,
    init_args: T::InitArgs,
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
        T::init(thread, common, init_args)
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
    critical_section(|| unsafe {
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
    critical_section(|| unsafe {
        let elapsed: u32 = elapsed_ticks_since_current_reload();
        let current_ktimer = if CURRENT_THREAD_IS_CFS {
            ptr::addr_of_mut!(CFS_KTIMER.entity)
        } else {
            rt_ktimer_entity(CURRENT_THREAD_CTX)
        };

        let next_ktimer = yield_ktimer(current_ktimer, elapsed, true);
        update_next_ktimer(next_ktimer);

        request_context_switch();
    });
}

pub fn msleepyi(msec: u32) {
    critical_section(|| unsafe {
        let elapsed = elapsed_ticks_since_current_reload();
        let current_ktimer = if CURRENT_THREAD_IS_CFS {
            ptr::addr_of_mut!(CFS_KTIMER.entity)
        } else {
            rt_ktimer_entity(CURRENT_THREAD_CTX)
        };

        let _ = yield_ktimer(current_ktimer, elapsed, false);

        let wait_entity = if CURRENT_THREAD_IS_CFS {
            cfs_wait_entity(CURRENT_THREAD_CTX)
        } else {
            rt_wait_entity(CURRENT_THREAD_CTX)
        };
        (*wait_entity).set_wake_after(ktimer_now_ticks(), msec.saturating_mul(ticks_per_ms()));
        (*wait_entity).waitevt = 0;

        if CURRENT_THREAD_IS_CFS {
            let _ = dequeue_runq_to_waitq(CURRENT_THREAD_CTX);
        } else {
            let _ = dequeue_ktimerq_to_waitq(CURRENT_THREAD_CTX);
        }

        request_context_switch();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ktimer::KTimerEntity;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn cfs_thread(name: &'static str, priority: u32) -> CfsThread {
        CfsThread {
            thread: ThreadCtx {
                sp: 0,
                exc_return: 0,
                id: 1,
                name,
                state: ThreadState::Ready,
                is_cfs: true,
            },
            wait_entity: WaitEntity::new(),
            sched_entity: SchedEntity::new(priority),
        }
    }

    fn rt_thread(name: &'static str) -> RtThread {
        RtThread {
            thread: ThreadCtx {
                sp: 0,
                exc_return: 0,
                id: 2,
                name,
                state: ThreadState::Ready,
                is_cfs: false,
            },
            wait_entity: WaitEntity::new(),
            ktimer_entity: ptr::null_mut(),
            runtime: 0,
        }
    }

    #[test]
    fn thread_ctx_exposes_cfs_sched_entity_only_for_cfs_threads() {
        let mut cfs = cfs_thread("cfs", 3);
        let rt = rt_thread("rt");

        assert!(cfs.thread.sched_entity().is_some());
        assert!(rt.thread.sched_entity().is_none());
        assert_eq!(cfs.thread.sched_entity().unwrap().priority, 3);

        unsafe {
            let entity = cfs_sched_entity(&mut cfs.thread);
            assert!(ptr::eq(
                thread_from_cfs_sched_entity(entity),
                &mut cfs.thread
            ));
        }
    }

    #[test]
    fn wait_info_selects_wait_entity_by_thread_class() {
        let mut cfs = cfs_thread("cfs", 1);
        let mut rt = rt_thread("rt");

        cfs.wait_entity.set_wake_after(0, 11);
        cfs.wait_entity.waitevt = 7;
        rt.wait_entity.set_wake_after(0, 13);
        rt.wait_entity.waitevt = 9;

        assert_eq!(cfs.thread.wait_info(), (11, 7));
        assert_eq!(rt.thread.wait_info(), (13, 9));
    }

    #[test]
    fn wait_entity_recovers_thread_context_for_both_thread_classes() {
        let mut cfs = cfs_thread("cfs", 1);
        let mut rt = rt_thread("rt");

        unsafe {
            assert!(ptr::eq(
                thread_from_wait_entity(cfs_wait_entity(&mut cfs.thread)),
                &mut cfs.thread
            ));
            assert!(ptr::eq(
                thread_from_wait_entity(rt_wait_entity(&mut rt.thread)),
                &mut rt.thread
            ));
        }
    }

    #[test]
    fn rt_thread_helpers_access_runtime_and_ktimer_entity() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut rt = rt_thread("rt");
        let mut ktimer = KTimerEntity::new(10);

        unsafe {
            CURRENT_THREAD_CTX = &mut rt.thread;
            CURRENT_THREAD_IS_CFS = false;

            set_rt_ktimer_entity(&mut rt.thread, &mut ktimer);
            assert!(ptr::eq(rt_ktimer_entity(&mut rt.thread), &mut ktimer));
            assert!(ptr::eq(rt_thread_from_thread_ctx(&mut rt.thread), &mut rt));
        }

        assert!(set_rt_thread_start_time(42));
        assert_eq!(rt.runtime, 42);

        unsafe {
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }

    #[test]
    fn set_rt_thread_start_time_ignores_cfs_current_thread() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut cfs = cfs_thread("cfs", 1);

        unsafe {
            CURRENT_THREAD_CTX = &mut cfs.thread;
            CURRENT_THREAD_IS_CFS = true;
        }

        assert!(!set_rt_thread_start_time(42));

        unsafe {
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }
}
