// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::ptr;

use crate::arch::ctx_swtich::request_context_switch;
use crate::ktimer::{
    CFS_KTIMER, CfsKTimer, KTimerEntity, advance_ktimers, dispatch_expired_ktimer,
    elapsed_ticks_since_last_interrupt, enqueue_ktimer, is_cfs_ktimer, next_ktimer,
    program_next_systick, update_next_ktimer,
};
use crate::runq::{CFS_RUN_QUEUE, SchedEntity, cfs_vruntime_delta, init_cfs_rq};
use crate::thread::{ThreadCtx, ThreadState, cfs_sched_entity, thread_from_cfs_sched_entity};

#[unsafe(no_mangle)]
pub static mut CURRENT_THREAD_CTX: *mut ThreadCtx = ptr::null_mut();
pub(crate) static mut CURRENT_THREAD_IS_CFS: bool = false;

/// Initialize the CFS scheduler state and enqueue its scheduler timer.
///
/// `ticks` is expressed in raw timer ticks because the board owns the clock
/// configuration.
pub unsafe fn init_cfs(period_ticks: u32, exec_ticks: u32) {
    unsafe {
        init_cfs_rq();
        CFS_KTIMER = CfsKTimer::new(period_ticks, exec_ticks, "cfs");
        let cfs_ktimer = (*ptr::addr_of_mut!(CFS_KTIMER)).entity_mut();
        (*cfs_ktimer).set_deadline(exec_ticks);
        enqueue_ktimer(cfs_ktimer);
    }
}

#[unsafe(no_mangle)]
extern "C" fn schedule() {
    unsafe {
        let next_ktimer = next_ktimer();
        if next_ktimer.is_null() {
            program_next_systick();
            return;
        }

        schedule_next(next_ktimer, elapsed_ticks_since_last_interrupt());
        program_next_systick();
    }
}

unsafe fn schedule_next(next_ktimer: *mut KTimerEntity, elapsed: u32) {
    unsafe {
        if next_ktimer.is_null() {
            return;
        }

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
        if CURRENT_THREAD_IS_CFS && (*CURRENT_THREAD_CTX).state == ThreadState::Running {
            let current_entity = cfs_sched_entity(CURRENT_THREAD_CTX);
            let sched_tick_added = u64::from(elapsed);
            let priority_sum = *CFS_RUN_QUEUE.priority_sum();
            if priority_sum == 0 {
                return;
            }
            (*current_entity).vruntime +=
                cfs_vruntime_delta(sched_tick_added, (*current_entity).priority, priority_sum);
            (*current_entity).sched_tick_cnt += sched_tick_added;
        }

        if is_cfs_ktimer(next_ktimer) {
            if let Some(next_entity) = (*CFS_RUN_QUEUE.get()).pop_first() {
                let next_thread = thread_from_cfs_sched_entity(next_entity as *mut SchedEntity);

                if CURRENT_THREAD_IS_CFS {
                    if (*CURRENT_THREAD_CTX).state == ThreadState::Waiting {
                        (*next_thread).state = ThreadState::Running;
                        CURRENT_THREAD_CTX = next_thread;
                        CURRENT_THREAD_IS_CFS = true;
                    } else {
                        let current_entity = cfs_sched_entity(CURRENT_THREAD_CTX);
                        debug_assert!(
                            CURRENT_THREAD_CTX != next_thread,
                            "CFS_RUN_QUEUE.pop_first() returned the CURRENT_THREAD_CTX running thread"
                        );
                        if (*current_entity).vruntime > (*next_entity).vruntime {
                            (*CURRENT_THREAD_CTX).state = ThreadState::Ready;
                            (*CFS_RUN_QUEUE.get()).insert(current_entity);
                            (*next_thread).state = ThreadState::Running;
                            CURRENT_THREAD_CTX = next_thread;
                            CURRENT_THREAD_IS_CFS = true;
                        } else {
                            (*CFS_RUN_QUEUE.get()).insert(next_entity as *mut SchedEntity);
                        }
                    }
                } else {
                    if (*CURRENT_THREAD_CTX).state != ThreadState::Waiting {
                        (*CURRENT_THREAD_CTX).state = ThreadState::Ready;
                    }
                    (*next_thread).state = ThreadState::Running;
                    CURRENT_THREAD_CTX = next_thread;
                    CURRENT_THREAD_IS_CFS = true;
                }
            }
        } else {
            let next_thread = (*KTimerEntity::rt_ktimer(next_ktimer)).thread_ctx();

            if next_thread.is_null() {
                return;
            }

            if (*CURRENT_THREAD_CTX).state != ThreadState::Waiting {
                (*CURRENT_THREAD_CTX).state = ThreadState::Ready;
                if CURRENT_THREAD_IS_CFS {
                    (*CFS_RUN_QUEUE.get()).insert(cfs_sched_entity(CURRENT_THREAD_CTX));
                }
            }
            (*next_thread).state = ThreadState::Running;
            CURRENT_THREAD_CTX = next_thread;
            CURRENT_THREAD_IS_CFS = false;
        }
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
/// - For RT KTimer, if active is true, it means current RT thread misses its deadline.
///   If active is false, current RT thread finishes its job before its deadline.
///   Both cases reset its deadline with duratioin and reactivate RT thread to be scheduled next.
pub fn handle_sched_tick() {
    let elapsed = elapsed_ticks_since_last_interrupt();

    let next_ktimer = unsafe {
        advance_ktimers(elapsed);
        dispatch_expired_ktimer(elapsed)
    };

    unsafe {
        update_next_ktimer(next_ktimer);
    }

    if !next_ktimer.is_null() {
        request_context_switch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ktimer::{RtKTimer, init_ktimer_queue};
    use crate::thread::{CfsThread, RtThread};
    use crate::waitq::WaitEntity;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn cfs_thread(
        name: &'static str,
        priority: u32,
        vruntime: u64,
        state: ThreadState,
    ) -> CfsThread {
        let mut thread = CfsThread {
            thread: ThreadCtx {
                sp: 0,
                exc_return: 0,
                id: 1,
                name,
                state,
                is_cfs: true,
            },
            wait_entity: WaitEntity::new(),
            sched_entity: SchedEntity::new(priority),
        };
        thread.sched_entity.vruntime = vruntime;
        thread
    }

    fn rt_thread(name: &'static str) -> RtThread {
        RtThread {
            thread: ThreadCtx {
                sp: 0,
                exc_return: 0,
                id: 2,
                name,
                state: ThreadState::Running,
                is_cfs: false,
            },
            wait_entity: WaitEntity::new(),
            ktimer_entity: ptr::null_mut(),
            runtime: 0,
        }
    }

    unsafe fn reset_sched_state() -> *mut KTimerEntity {
        unsafe {
            init_cfs_rq();
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
            CFS_KTIMER = CfsKTimer::new(100, 25, "cfs");
            (*ptr::addr_of_mut!(CFS_KTIMER)).entity_mut()
        }
    }

    unsafe fn queue_cfs_thread(thread: *mut ThreadCtx) {
        unsafe {
            let entity = cfs_sched_entity(thread);
            (*entity).reset_links();
            (*CFS_RUN_QUEUE.get()).insert(entity);
            *CFS_RUN_QUEUE.priority_sum() += (*entity).priority;
        }
    }

    unsafe fn running_thread_count(threads: &[*const ThreadCtx]) -> usize {
        threads
            .iter()
            .filter(|&&thread| {
                !thread.is_null() && unsafe { (*thread).state == ThreadState::Running }
            })
            .count()
    }

    #[test]
    fn init_cfs_resets_run_queue_and_configures_cfs_timer() {
        let _guard = TEST_LOCK.lock().unwrap();

        unsafe {
            init_ktimer_queue();
            init_cfs(100, 25);
        }

        unsafe {
            assert_eq!((*CFS_RUN_QUEUE.get()).len(), 0);
            assert_eq!(*CFS_RUN_QUEUE.priority_sum(), 0);
            let cfs = ptr::addr_of!(CFS_KTIMER);
            let entity = ptr::addr_of!((*cfs).entity);
            assert_eq!((*entity).duration(), 100);
            assert_eq!((*entity).deadline(), 25);
            assert_eq!((*cfs).execution_ticks(), 25);
            assert!((*entity).is_active());
        }
    }

    #[test]
    fn cfs_accounting_updates_vruntime_and_sched_ticks() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut current = cfs_thread("current", 1, 0, ThreadState::Running);
        let mut queued = cfs_thread("queued", 3, 100, ThreadState::Ready);

        unsafe {
            let cfs_ktimer = reset_sched_state();
            CURRENT_THREAD_CTX = &mut current.thread;
            CURRENT_THREAD_IS_CFS = true;
            queue_cfs_thread(&mut queued.thread);
            *CFS_RUN_QUEUE.priority_sum() += current.sched_entity.priority;

            schedule_next(cfs_ktimer, 12);
        }

        assert_eq!(current.sched_entity.sched_tick_cnt(), 12);
        assert_eq!(current.sched_entity.vruntime(), 3);
        assert!(current.thread.state == ThreadState::Running);
    }

    #[test]
    fn cfs_preempts_current_when_queued_thread_has_lower_vruntime() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut current = cfs_thread("current", 2, 10, ThreadState::Running);
        let mut queued = cfs_thread("queued", 2, 5, ThreadState::Ready);

        unsafe {
            let cfs_ktimer = reset_sched_state();
            CURRENT_THREAD_CTX = &mut current.thread;
            CURRENT_THREAD_IS_CFS = true;
            queue_cfs_thread(&mut queued.thread);
            *CFS_RUN_QUEUE.priority_sum() += current.sched_entity.priority;

            schedule_next(cfs_ktimer, 10);
        }

        assert_eq!(current.sched_entity.vruntime(), 15);
        assert!(current.thread.state == ThreadState::Ready);
        assert!(queued.thread.state == ThreadState::Running);
        assert_eq!(
            unsafe { running_thread_count(&[&current.thread, &queued.thread]) },
            1
        );
        unsafe {
            assert!(ptr::eq(CURRENT_THREAD_CTX, &mut queued.thread));
            assert!(CURRENT_THREAD_IS_CFS);
            assert!(ptr::eq(
                (*CFS_RUN_QUEUE.get()).first(),
                &mut current.sched_entity
            ));
        }
    }

    #[test]
    fn cfs_keeps_current_when_it_still_has_lower_vruntime() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut current = cfs_thread("current", 2, 0, ThreadState::Running);
        let mut queued = cfs_thread("queued", 2, 10, ThreadState::Ready);

        unsafe {
            let cfs_ktimer = reset_sched_state();
            CURRENT_THREAD_CTX = &mut current.thread;
            CURRENT_THREAD_IS_CFS = true;
            queue_cfs_thread(&mut queued.thread);
            *CFS_RUN_QUEUE.priority_sum() += current.sched_entity.priority;

            schedule_next(cfs_ktimer, 2);
        }

        assert_eq!(current.sched_entity.vruntime(), 1);
        assert!(current.thread.state == ThreadState::Running);
        assert!(queued.thread.state == ThreadState::Ready);
        assert_eq!(
            unsafe { running_thread_count(&[&current.thread, &queued.thread]) },
            1
        );
        unsafe {
            assert!(ptr::eq(CURRENT_THREAD_CTX, &mut current.thread));
            assert!(CURRENT_THREAD_IS_CFS);
            assert!(ptr::eq(
                (*CFS_RUN_QUEUE.get()).first(),
                &mut queued.sched_entity
            ));
            assert!(!current.sched_entity.is_linked());
        }
    }

    #[test]
    fn cfs_timer_switches_from_rt_thread_to_leftmost_cfs_thread() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut rt = rt_thread("rt");
        let mut first = cfs_thread("first", 1, 30, ThreadState::Ready);
        let mut second = cfs_thread("second", 1, 10, ThreadState::Ready);

        unsafe {
            let cfs_ktimer = reset_sched_state();
            CURRENT_THREAD_CTX = &mut rt.thread;
            CURRENT_THREAD_IS_CFS = false;
            queue_cfs_thread(&mut first.thread);
            queue_cfs_thread(&mut second.thread);

            schedule_next(cfs_ktimer, 0);
        }

        assert!(rt.thread.state == ThreadState::Ready);
        assert!(second.thread.state == ThreadState::Running);
        assert_eq!(
            unsafe { running_thread_count(&[&rt.thread, &first.thread, &second.thread]) },
            1
        );
        unsafe {
            assert!(ptr::eq(CURRENT_THREAD_CTX, &mut second.thread));
            assert!(CURRENT_THREAD_IS_CFS);
            assert!(ptr::eq(
                (*CFS_RUN_QUEUE.get()).first(),
                &mut first.sched_entity
            ));
        }
    }

    #[test]
    fn rt_timer_switches_from_cfs_thread_to_rt_thread_and_requeues_cfs() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut current = cfs_thread("cfs", 2, 4, ThreadState::Running);
        let mut rt = rt_thread("rt");
        let mut rt_ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

        unsafe {
            let _ = reset_sched_state();
            rt_ktimer.init_rt_ktimer(&mut rt.thread);
            CURRENT_THREAD_CTX = &mut current.thread;
            CURRENT_THREAD_IS_CFS = true;
            *CFS_RUN_QUEUE.priority_sum() = current.sched_entity.priority;

            schedule_next(rt_ktimer.entity_mut(), 0);
        }

        assert!(current.thread.state == ThreadState::Ready);
        assert!(rt.thread.state == ThreadState::Running);
        assert_eq!(
            unsafe { running_thread_count(&[&current.thread, &rt.thread]) },
            1
        );
        unsafe {
            assert!(ptr::eq(CURRENT_THREAD_CTX, &mut rt.thread));
            assert!(!CURRENT_THREAD_IS_CFS);
            assert!(ptr::eq(
                (*CFS_RUN_QUEUE.get()).first(),
                &mut current.sched_entity
            ));
        }
    }
}
