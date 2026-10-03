// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Host-only hooks used by integration tests.

extern crate std;

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr;

use crate::arch::platform::THREAD_INITIAL_FRAME_WORDS;
use crate::diagnostics::trace;
use crate::ktimer::{KTimerEntity, RtKTimer};
use crate::runq::{CFS_RUN_QUEUE, SchedEntity};
use crate::sched::{CURRENT_THREAD_CTX, CURRENT_THREAD_IS_CFS};
use crate::sync::{SyncEntity, SyncType};
use crate::thread::{
    AlignedStack, CfsThread, IdleThread, RtThread, ThreadCtx, ThreadEntry, ThreadHandle,
    ThreadKind, ThreadStart, ThreadState, cfs_sched_entity, cfs_sync_entity, cfs_wait_entity,
    forkyi, rt_ktimer_entity, rt_sync_entity, rt_thread_from_handle, rt_wait_entity,
    set_rt_ktimer_entity, spawn_idle_thread, sync_entity, thread_handle_from_cfs_sched_entity,
    thread_handle_from_sync_entity, thread_handle_from_wait_entity,
};
use crate::waitq::WaitEntity;

pub const INITIAL_THREAD_FRAME_WORDS: usize = THREAD_INITIAL_FRAME_WORDS;

pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static TEST_CRITICAL_SECTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

std::thread_local! {
    static TEST_CRITICAL_SECTION_DEPTH: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

pub fn critical_section<R>(f: impl FnOnce() -> R) -> R {
    TEST_CRITICAL_SECTION_DEPTH.with(|depth| {
        if depth.get() > 0 {
            depth.set(depth.get() + 1);
            let result = f();
            depth.set(depth.get() - 1);
            return result;
        }

        let _lock = TEST_CRITICAL_SECTION_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        depth.set(1);
        let result = f();
        depth.set(0);
        result
    })
}

pub fn clear_print_fn_for_test() {
    crate::print::clear_print_fn_for_test();
}

pub use crate::ktimer::test_support as ktimer;
pub use crate::rbtree::test_support as rbtree;
pub use crate::runq::test_support as runq;
pub use crate::sched::test_support as sched;
pub use crate::sync::test_support as sync;
pub use crate::waitq::test_support as waitq;

pub fn trace_thread(id: u32, name: &'static str, is_cfs: bool) -> ThreadCtx {
    ThreadCtx {
        sp: 0,
        exc_return: 0,
        id,
        name,
        state: ThreadState::Ready,
        kind: if is_cfs {
            ThreadKind::Cfs
        } else {
            ThreadKind::Rt
        },
    }
}

pub fn record_context_switch_for_test(from: *const ThreadCtx, to: *const ThreadCtx) {
    trace::record_context_switch(from, to);
}

pub fn record_deadline_miss_for_test(
    thread: *const ThreadCtx,
    runtime_ticks: u32,
    relative_deadline_ticks: u32,
) {
    trace::record_deadline_miss(thread, runtime_ticks, relative_deadline_ticks);
}

pub fn record_wakeup_for_test(thread: *const ThreadCtx) {
    trace::record_wakeup(thread);
}

pub fn record_yield_for_test(thread: *const ThreadCtx, elapsed_ticks: u32) {
    trace::record_yield(thread, elapsed_ticks);
}

#[cfg(feature = "sched-isr-timing")]
pub unsafe fn set_sched_tick_timing_for_test(
    last_ticks: u32,
    min_ticks: u32,
    max_ticks: u32,
    samples: u32,
    advance_max_ticks: u32,
    dispatch_max_ticks: u32,
) {
    unsafe {
        ptr::write_volatile(&raw mut trace::SCHED_TICK_TO_PENDSV_LAST_TICKS, last_ticks);
        ptr::write_volatile(&raw mut trace::SCHED_TICK_TO_PENDSV_MIN_TICKS, min_ticks);
        ptr::write_volatile(&raw mut trace::SCHED_TICK_TO_PENDSV_MAX_TICKS, max_ticks);
        ptr::write_volatile(&raw mut trace::SCHED_TICK_TO_PENDSV_SAMPLES, samples);
        ptr::write_volatile(
            &raw mut trace::SCHED_TICK_ADVANCE_KTIMERS_MAX_TICKS,
            advance_max_ticks,
        );
        ptr::write_volatile(
            &raw mut trace::SCHED_TICK_DISPATCH_EXPIRED_KTIMER_MAX_TICKS,
            dispatch_max_ticks,
        );
    }
}

pub fn cfs_thread(name: &'static str, priority: u32) -> CfsThread {
    CfsThread {
        thread: ThreadCtx {
            sp: 0,
            exc_return: 0,
            id: 1,
            name,
            state: ThreadState::Ready,
            kind: ThreadKind::Cfs,
        },
        wait_entity: WaitEntity::new(),
        sync_entity: SyncEntity::new(),
        sched_entity: SchedEntity::new(priority),
    }
}

pub fn rt_thread(name: &'static str) -> RtThread {
    RtThread {
        thread: ThreadCtx {
            sp: 0,
            exc_return: 0,
            id: 2,
            name,
            state: ThreadState::Ready,
            kind: ThreadKind::Rt,
        },
        wait_entity: WaitEntity::new(),
        sync_entity: SyncEntity::new(),
        ktimer_entity: ptr::null_mut(),
        runtime: 0,
    }
}

pub unsafe fn thread_handle(thread: &mut ThreadCtx) -> ThreadHandle {
    unsafe { ThreadHandle::from_thread_ctx(thread) }
}

pub fn thread_ptr(thread: ThreadHandle) -> *mut ThreadCtx {
    thread.as_ptr()
}

pub fn set_thread_state(thread: &mut ThreadCtx, state: ThreadState) {
    thread.set_state(state);
}

pub unsafe fn init_cfs_run_queue() {
    unsafe {
        crate::runq::init_cfs_rq();
    }
}

pub fn cfs_run_queue_len() -> usize {
    unsafe { (*CFS_RUN_QUEUE.get()).len() }
}

pub fn cfs_run_queue_priority_sum() -> u32 {
    unsafe { *CFS_RUN_QUEUE.priority_sum() }
}

pub unsafe fn spawn_idle_thread_for_test<const N: usize>(
    start: ThreadStart,
    thread: *mut MaybeUninit<IdleThread>,
    stack: *mut AlignedStack<N>,
) -> ThreadHandle {
    unsafe { spawn_idle_thread(start, thread, stack) }
}

pub unsafe fn raw_forkyi_cfs(
    thread: *mut CfsThread,
    sp: *mut u32,
    entry: ThreadEntry,
    arg: *mut c_void,
    name: &'static str,
    priority: u32,
) -> ThreadHandle {
    unsafe { forkyi::<CfsThread>(thread, sp, entry, arg, name, priority) }
}

pub unsafe fn cfs_sched_entity_owner_addr(thread: &mut CfsThread) -> usize {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(thread.thread_ctx_mut());
        let entity = cfs_sched_entity(handle);
        thread_handle_from_cfs_sched_entity(entity).as_ptr() as usize
    }
}

pub fn set_cfs_wait_info(thread: &mut CfsThread, wait_ticks: u32, waitevt: Option<SyncType>) {
    thread.wait_entity.set_wake_after(0, wait_ticks);
    thread.wait_entity.waitevt = waitevt;
}

pub fn set_rt_wait_info(thread: &mut RtThread, wait_ticks: u32, waitevt: Option<SyncType>) {
    thread.wait_entity.set_wake_after(0, wait_ticks);
    thread.wait_entity.waitevt = waitevt;
}

pub unsafe fn cfs_wait_entity_owner_addr(thread: &mut CfsThread) -> usize {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(thread.thread_ctx_mut());
        thread_handle_from_wait_entity(cfs_wait_entity(handle)).as_ptr() as usize
    }
}

pub unsafe fn rt_wait_entity_owner_addr(thread: &mut RtThread) -> usize {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(thread.thread_ctx_mut());
        thread_handle_from_wait_entity(rt_wait_entity(handle)).as_ptr() as usize
    }
}

pub unsafe fn cfs_sync_entity_owner_addr(thread: &mut CfsThread) -> usize {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(thread.thread_ctx_mut());
        thread_handle_from_sync_entity(cfs_sync_entity(handle)).as_ptr() as usize
    }
}

pub unsafe fn rt_sync_entity_owner_addr(thread: &mut RtThread) -> usize {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(thread.thread_ctx_mut());
        thread_handle_from_sync_entity(rt_sync_entity(handle)).as_ptr() as usize
    }
}

pub unsafe fn sync_entity_owner_addr(thread: &mut ThreadCtx) -> usize {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(thread);
        thread_handle_from_sync_entity(sync_entity(handle)).as_ptr() as usize
    }
}

pub unsafe fn set_current_thread(thread: *mut ThreadCtx, is_cfs: bool) {
    unsafe {
        CURRENT_THREAD_CTX = thread;
        CURRENT_THREAD_IS_CFS = is_cfs;
    }
}

pub unsafe fn clear_current_thread() {
    unsafe {
        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub unsafe fn attach_rt_ktimer(thread: &mut RtThread, ktimer: &mut RtKTimer) -> ThreadHandle {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(thread.thread_ctx_mut());
        set_rt_ktimer_entity(handle, rt_ktimer_entity_ptr(ktimer));
        handle
    }
}

pub fn rt_ktimer_entity_addr(ktimer: &mut RtKTimer) -> usize {
    rt_ktimer_entity_ptr(ktimer) as usize
}

pub fn rt_ktimer_thread_ctx_addr(ktimer: &RtKTimer) -> usize {
    ktimer.thread_ctx() as usize
}

pub unsafe fn rt_ktimer_entity_addr_from_handle(thread: ThreadHandle) -> usize {
    unsafe { rt_ktimer_entity(thread) as usize }
}

pub unsafe fn rt_thread_addr_from_handle(thread: ThreadHandle) -> usize {
    unsafe { rt_thread_from_handle(thread) as usize }
}

fn rt_ktimer_entity_ptr(ktimer: &mut RtKTimer) -> *mut KTimerEntity {
    ktimer.entity_mut()
}
