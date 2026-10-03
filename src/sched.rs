// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::ptr;

use crate::arch::platform::request_context_switch;
use crate::ktimer::{
    CFS_KTIMER, CfsKTimer, KTimerEntity, advance_ktimers, dispatch_expired_ktimer,
    elapsed_ticks_since_last_interrupt, enqueue_ktimer, is_cfs_ktimer, next_ktimer,
    program_next_scheduler_timer, update_next_ktimer,
};
use crate::runq::{CFS_RUN_QUEUE, SchedEntity, cfs_vruntime_delta, init_cfs_rq};
use crate::thread::{
    ThreadCtx, ThreadHandle, ThreadState, cfs_sched_entity, thread_handle_from_cfs_sched_entity,
};

#[unsafe(no_mangle)]
pub static mut CURRENT_THREAD_CTX: *mut ThreadCtx = ptr::null_mut();
pub(crate) static mut CURRENT_THREAD_IS_CFS: bool = false;
pub(crate) static mut IDLE_THREAD_CTX: *mut ThreadCtx = ptr::null_mut();
#[unsafe(no_mangle)]
pub static mut SCHEDULER_STARTED: u32 = 0;

pub(crate) fn reset_scheduler_started() {
    unsafe {
        ptr::write_volatile(&raw mut SCHEDULER_STARTED, 0);
    }
}

fn scheduler_started() -> bool {
    unsafe { ptr::read_volatile(&raw const SCHEDULER_STARTED) != 0 }
}

#[cfg(not(target_arch = "arm"))]
fn mark_scheduler_started() {
    unsafe {
        ptr::write_volatile(&raw mut SCHEDULER_STARTED, 1);
    }
}

/// Initialize the CFS scheduler state and enqueue its scheduler timer.
///
/// `ticks` is expressed in raw timer ticks because the board owns the clock
/// configuration.
///
/// # Safety
///
/// Call this during single-threaded scheduler setup after `init_ktimer_queue`
/// and before any scheduler interrupt, thread, or other core can observe the
/// global CFS state.
///
/// Do not call this while CFS threads are queued, running, or waiting.
/// Reinitializing with live CFS entities invalidates their intrusive run-queue
/// links and loses scheduler accounting.
pub unsafe fn init_cfs(period_ticks: u32, exec_ticks: u32) {
    unsafe {
        reset_scheduler_started();
        init_cfs_rq();
        IDLE_THREAD_CTX = ptr::null_mut();
        CFS_KTIMER = CfsKTimer::new(period_ticks, exec_ticks, "cfs");
        let cfs_ktimer = (*ptr::addr_of_mut!(CFS_KTIMER)).entity_mut();
        (*cfs_ktimer).set_expire(exec_ticks);
        enqueue_ktimer(cfs_ktimer);
    }
}

/// Register the thread that should run when no normal work is runnable.
///
/// The idle thread is a scheduler fallback only. It is neither a CFS nor RT
/// thread and must never participate in CFS fairness accounting or RT timer
/// scheduling.
///
/// # Safety
///
/// `thread` must refer to a live `IdleThread`. The thread should be newly
/// spawned or otherwise quiescent: it must not be the currently running thread,
/// must not be waiting, and must not also be used as normal runnable work.
///
/// The thread's backing storage and stack must outlive all scheduler use. Call
/// this after `init_cfs`, before starting the scheduler, and while scheduler
/// state is not being concurrently modified.
pub unsafe fn register_idle_thread(thread: ThreadHandle) {
    let thread_ptr = thread.as_ptr();

    crate::critical_section(|| unsafe {
        assert!((*thread_ptr).is_idle(), "idle thread must be an IdleThread");

        IDLE_THREAD_CTX = thread_ptr;
    });
}

pub(crate) unsafe fn is_idle_thread(thread: *const ThreadCtx) -> bool {
    unsafe { !thread.is_null() && thread == IDLE_THREAD_CTX.cast_const() }
}

pub fn traverse_idle_thread_fn<F>(mut f: F)
where
    F: FnMut(&ThreadCtx),
{
    crate::critical_section(|| unsafe {
        if !IDLE_THREAD_CTX.is_null() {
            f(&*IDLE_THREAD_CTX);
        }
    });
}

unsafe fn switch_to_cfs_thread(next_thread: ThreadHandle) {
    unsafe {
        let next_thread_ptr = next_thread.as_ptr();

        crate::diagnostics::trace::record_context_switch(CURRENT_THREAD_CTX, next_thread_ptr);

        if !CURRENT_THREAD_CTX.is_null()
            && CURRENT_THREAD_CTX != next_thread_ptr
            && (*CURRENT_THREAD_CTX).state != ThreadState::Waiting
        {
            (*CURRENT_THREAD_CTX).set_state(ThreadState::Ready);
        }

        (*next_thread_ptr).set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = next_thread_ptr;
        CURRENT_THREAD_IS_CFS = true;
    }
}

unsafe fn switch_to_idle_thread() {
    unsafe {
        let idle_thread_ptr = IDLE_THREAD_CTX;
        if idle_thread_ptr.is_null() {
            return;
        }

        crate::diagnostics::trace::record_context_switch(CURRENT_THREAD_CTX, idle_thread_ptr);

        if !CURRENT_THREAD_CTX.is_null()
            && CURRENT_THREAD_CTX != idle_thread_ptr
            && (*CURRENT_THREAD_CTX).state != ThreadState::Waiting
        {
            (*CURRENT_THREAD_CTX).set_state(ThreadState::Ready);
        }

        (*idle_thread_ptr).set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = idle_thread_ptr;
        CURRENT_THREAD_IS_CFS = (*idle_thread_ptr).is_cfs();
    }
}

unsafe fn requeue_current_cfs_thread_before_idle() {
    unsafe {
        if IDLE_THREAD_CTX.is_null()
            || CURRENT_THREAD_CTX.is_null()
            || !CURRENT_THREAD_IS_CFS
            || is_idle_thread(CURRENT_THREAD_CTX)
            || (*CURRENT_THREAD_CTX).state == ThreadState::Waiting
        {
            return;
        }

        let current = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
        let current_entity = cfs_sched_entity(current);
        debug_assert!(
            !(*CFS_RUN_QUEUE.get()).contains(current_entity.cast_const()),
            "running CFS thread is already queued"
        );
        (*CURRENT_THREAD_CTX).set_state(ThreadState::Ready);
        (*CFS_RUN_QUEUE.get()).insert(current_entity);
    }
}

unsafe fn switch_to_idle_thread_with_current_requeued() {
    unsafe {
        requeue_current_cfs_thread_before_idle();
        switch_to_idle_thread();
    }
}

#[unsafe(no_mangle)]
extern "C" fn schedule() {
    unsafe {
        let next_ktimer = next_ktimer();
        let elapsed = elapsed_ticks_since_last_interrupt();

        // The scheduler logic is as follows:
        // - If the CURRENT_THREAD_CTX is CFS, update its vruntime based on the elapsed
        //   ticks and its inverse-numeric priority. Lower numeric priority values are
        //   favored because they accumulate vruntime more slowly.
        // - If the next expired ktimer is for a CFS thread and current thread is
        //   CFS thread, compare its vruntime with the CURRENT_THREAD_CTX's vruntime
        //   to decide whether to preempt.
        // - If the next expired ktimer is for a CFS thread and current thread is
        //   RT thread, switch to the left-most CFS thread.
        // - If the next expired ktimer is for an RT thread and current thread is
        //   CFS thread, insert current to CFS runq and switch to next RT thread.
        // - If the next expired ktimer is for an RT thread and current thread is
        //   RT thread, preempt the CURRENT_THREAD_CTX with next RT thread.
        if CURRENT_THREAD_IS_CFS
            && (*CURRENT_THREAD_CTX).state == ThreadState::Running
            && !is_idle_thread(CURRENT_THREAD_CTX)
        {
            let current = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
            let current_entity = cfs_sched_entity(current);
            let sched_tick_added = u64::from(elapsed);
            let priority_sum = *CFS_RUN_QUEUE.priority_sum();
            if priority_sum != 0 {
                (*current_entity).vruntime +=
                    cfs_vruntime_delta(sched_tick_added, (*current_entity).priority, priority_sum);
                (*current_entity).sched_tick_cnt += sched_tick_added;
            }
        }

        if next_ktimer.is_null() {
            switch_to_idle_thread_with_current_requeued();
        } else if is_cfs_ktimer(next_ktimer) {
            if !(*next_ktimer).is_active() {
                switch_to_idle_thread_with_current_requeued();
            } else if let Some(next_entity) = (*CFS_RUN_QUEUE.get()).pop_first() {
                let next_thread =
                    thread_handle_from_cfs_sched_entity(next_entity as *mut SchedEntity);
                let next_thread_ptr = next_thread.as_ptr();

                if CURRENT_THREAD_IS_CFS {
                    if (*CURRENT_THREAD_CTX).state == ThreadState::Waiting
                        || is_idle_thread(CURRENT_THREAD_CTX)
                    {
                        switch_to_cfs_thread(next_thread);
                    } else {
                        let current = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
                        let current_entity = cfs_sched_entity(current);
                        debug_assert!(
                            CURRENT_THREAD_CTX != next_thread_ptr,
                            "CFS_RUN_QUEUE.pop_first() returned the CURRENT_THREAD_CTX running thread"
                        );
                        if (*current_entity).vruntime > next_entity.vruntime {
                            crate::diagnostics::trace::record_context_switch(
                                CURRENT_THREAD_CTX,
                                next_thread_ptr,
                            );
                            (*CURRENT_THREAD_CTX).set_state(ThreadState::Ready);
                            (*CFS_RUN_QUEUE.get()).insert(current_entity);
                            (*next_thread_ptr).set_state(ThreadState::Running);
                            CURRENT_THREAD_CTX = next_thread_ptr;
                            CURRENT_THREAD_IS_CFS = true;
                        } else {
                            (*CFS_RUN_QUEUE.get()).insert(next_entity as *mut SchedEntity);
                        }
                    }
                } else {
                    if (*CURRENT_THREAD_CTX).state != ThreadState::Waiting {
                        (*CURRENT_THREAD_CTX).set_state(ThreadState::Ready);
                    }
                    switch_to_cfs_thread(next_thread);
                }
            } else {
                switch_to_idle_thread_with_current_requeued();
            }
        } else {
            let next_thread = (*KTimerEntity::container_of(next_ktimer)).thread_ctx();

            if next_thread.is_null() {
                return;
            }
            let next_thread_handle = ThreadHandle::from_thread_ctx(next_thread);

            if (*CURRENT_THREAD_CTX).state != ThreadState::Waiting {
                (*CURRENT_THREAD_CTX).set_state(ThreadState::Ready);
                if CURRENT_THREAD_IS_CFS && !is_idle_thread(CURRENT_THREAD_CTX) {
                    let current = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
                    (*CFS_RUN_QUEUE.get()).insert(cfs_sched_entity(current));
                }
            }
            crate::diagnostics::trace::record_context_switch(CURRENT_THREAD_CTX, next_thread);
            (*next_thread_handle.as_ptr()).set_state(ThreadState::Running);
            CURRENT_THREAD_CTX = next_thread_handle.as_ptr();
            CURRENT_THREAD_IS_CFS = false;
        }

        program_next_scheduler_timer();
    }
}

/// Handle one scheduler tick and request ktimer dispatch.
///
/// A scheduler tick means different things for each KTimer type:
/// - For CFS KTimer, it means the current CFS KTimer has exhausted its execution time slice,
///   and does the context switch to the thread of next earliest deadline KTimer (KTimer of
///   RT thread, CFS_KTIMER or WAIT_KTIMER) that should preempt the current thread.
/// - For wait KTimer, it means there is a WAITING thread in the WAIT_QUEUE that needs to be
///   woken up and moved to the runq and should be scheduled.
/// - For RT KTimer, if its active is true, it means current RT thread misses its deadline.
///   If its active is false, current RT thread finishes its job before its deadline.
///   Active RT timers are re-armed with their relative deadline; inactive RT timers
///   are reactivated at their next period release.
pub fn handle_sched_tick() {
    if !scheduler_started() {
        return;
    }

    let elapsed = elapsed_ticks_since_last_interrupt();

    let next_ktimer = unsafe {
        #[cfg(feature = "sched-isr-timing")]
        let ktimer_start_cycle = crate::arch::platform::dwt_cycle_count();

        advance_ktimers(elapsed);

        #[cfg(feature = "sched-isr-timing")]
        let after_advance_cycle = crate::arch::platform::dwt_cycle_count();

        let next_ktimer = dispatch_expired_ktimer(elapsed);

        #[cfg(feature = "sched-isr-timing")]
        {
            let after_dispatch_cycle = crate::arch::platform::dwt_cycle_count();
            crate::diagnostics::trace::record_sched_tick_ktimer_timing(
                after_advance_cycle.wrapping_sub(ktimer_start_cycle),
                after_dispatch_cycle.wrapping_sub(after_advance_cycle),
            );
        }

        next_ktimer
    };

    unsafe {
        update_next_ktimer(next_ktimer);
    }

    crate::diagnostics::trace::arm_sched_tick_to_pendsv_sample();
    request_context_switch();
}

#[cfg(not(target_arch = "arm"))]
#[path = "../tests/support/sched.rs"]
pub mod test_support;
