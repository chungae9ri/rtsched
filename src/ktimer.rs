// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Kernel timer queue keyed by ktimer expiration time.
//!
//! Active and inactive timers occupy separate trees with cached minima.
//! Runnable selection reads the active minimum, while timer programming and
//! expiry dispatch compare both minima so inactive releases and wakes still fire.
//!
//! The queue is intrusive: each `KTimerEntity` embeds its own `RbNode`, so
//! inserting ktimers does not allocate.

use core::cell::UnsafeCell;
use core::cmp::Ordering;
use core::mem::offset_of;
use core::ptr;
#[cfg(not(target_arch = "arm"))]
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering as AtomicOrdering};

use crate::arch::platform;
use crate::arch::platform::{SCHEDULER_TIMER_RELOAD_MAX, SCHEDULER_TIMER_RELOAD_MIN};
use crate::critical_section;
use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::runq::enqueue_runq_from_waitq;
use crate::thread::{
    RtThread, ThreadCtx, ThreadHandle, ThreadKind, ThreadState, rt_ktimer_entity,
    rt_thread_from_handle, set_rt_ktimer_entity,
};
use crate::waitq::{
    WAIT_QUEUE, WaitQueueError, insert_wait_thread, pop_expired_wait_thread, remove_wait_thread,
};

const KTIMER_EXPIRE_NEVER: u64 = u64::MAX;
static KTIMER_QUEUE: GlobalKTimerQueue = GlobalKTimerQueue::new();
static mut NEXT_KTIMER: *mut KTimerEntity = ptr::null_mut();
pub(crate) static mut CFS_KTIMER: CfsKTimer = CfsKTimer::new(0, 0, "cfs");
pub(crate) static mut WAIT_KTIMER: WaitKTimer = WaitKTimer::new();

#[cfg(not(target_arch = "arm"))]
static TEST_ELAPSED_TICKS_OVERRIDE_SET: AtomicBool = AtomicBool::new(false);
#[cfg(not(target_arch = "arm"))]
static TEST_ELAPSED_TICKS_OVERRIDE: AtomicU32 = AtomicU32::new(0);

struct GlobalKTimerQueue {
    queue: UnsafeCell<KTimerQueue>,
}

impl GlobalKTimerQueue {
    const fn new() -> Self {
        Self {
            queue: UnsafeCell::new(KTimerQueue::new()),
        }
    }

    fn get(&self) -> *mut KTimerQueue {
        self.queue.get()
    }
}

unsafe impl Sync for GlobalKTimerQueue {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtTiming {
    /// Time between releases of consecutive jobs, when the next job is released.
    period_ticks: u32,
    /// Relative deadline for each released job, when this job must complete by.
    relative_deadline_ticks: u32,
    /// Maximum runtime budget charged to the current job window.
    /// How much CPU time it is allowed to consume.
    budget_ticks: u32,
}

impl RtTiming {
    pub const fn new(period_ticks: u32, relative_deadline_ticks: u32, budget_ticks: u32) -> Self {
        Self {
            period_ticks,
            relative_deadline_ticks,
            budget_ticks,
        }
    }

    pub const fn from_period(period_ticks: u32) -> Self {
        Self::new(period_ticks, period_ticks, period_ticks)
    }

    pub const fn period_ticks(&self) -> u32 {
        self.period_ticks
    }

    pub const fn relative_deadline_ticks(&self) -> u32 {
        self.relative_deadline_ticks
    }

    pub const fn budget_ticks(&self) -> u32 {
        self.budget_ticks
    }
}

#[repr(C)]
pub(crate) struct KTimerEntity {
    expire_at: u64,
    node: RbNode,
    active: bool,
    timing: RtTiming,
}

impl KTimerEntity {
    #[cfg(not(target_arch = "arm"))]
    pub const fn new(expire_ticks: u32) -> Self {
        Self::new_with_timing(expire_ticks, RtTiming::from_period(expire_ticks))
    }

    pub const fn new_with_timing(expire_ticks: u32, timing: RtTiming) -> Self {
        Self {
            expire_at: expire_ticks as u64,
            node: RbNode::new(),
            active: true,
            timing,
        }
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn expire(&self) -> u32 {
        self.expire_at.min(u64::from(u32::MAX)) as u32
    }

    pub fn set_expire(&mut self, expire: u32) {
        self.expire_at = u64::from(expire);
    }

    pub fn expire_at(&self) -> u64 {
        self.expire_at
    }

    pub fn set_expire_at(&mut self, expire_at: u64) {
        self.expire_at = expire_at;
    }

    pub fn set_expire_after(&mut self, now_ticks: u64, ticks: u32) {
        self.expire_at = now_ticks.saturating_add(u64::from(ticks));
    }

    pub fn set_expire_never(&mut self) {
        self.expire_at = KTIMER_EXPIRE_NEVER;
    }

    pub fn remaining_at(&self, now_ticks: u64) -> u32 {
        if self.expire_at == KTIMER_EXPIRE_NEVER {
            return SCHEDULER_TIMER_RELOAD_MAX;
        }

        self.expire_at
            .saturating_sub(now_ticks)
            .min(u64::from(SCHEDULER_TIMER_RELOAD_MAX)) as u32
    }

    pub fn is_expired_at(&self, now_ticks: u64) -> bool {
        self.expire_at <= now_ticks
    }

    pub fn reset_links(&mut self) {
        self.node.reset_links();
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn is_linked(&self) -> bool {
        self.node.is_linked()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn timing(&self) -> RtTiming {
        self.timing
    }

    pub fn period_ticks(&self) -> u32 {
        self.timing.period_ticks()
    }

    pub fn relative_deadline_ticks(&self) -> u32 {
        self.timing.relative_deadline_ticks()
    }

    pub fn budget_ticks(&self) -> u32 {
        self.timing.budget_ticks()
    }

    /// Recover the owning RT timer from its embedded timer entity.
    ///
    /// # Safety
    ///
    /// `entity` must be non-null and must point to the `entity` field of a
    /// live `RtKTimer`. It must not point to the global CFS timer, the global
    /// wait timer, or any other allocation with a `KTimerEntity` layout.
    pub unsafe fn container_of(entity: *mut Self) -> *mut RtKTimer {
        debug_assert!(!entity.is_null());
        debug_assert!(!is_cfs_ktimer(entity));
        debug_assert!(!is_wait_ktimer(entity));

        (entity as *mut u8)
            .wrapping_sub(offset_of!(RtKTimer, entity))
            .cast::<RtKTimer>()
    }
}

#[repr(C)]
pub(crate) struct CfsKTimer {
    pub entity: KTimerEntity,
    pub name: &'static str,
}

impl CfsKTimer {
    pub const fn new(period_ticks: u32, execution_ticks: u32, name: &'static str) -> Self {
        Self {
            entity: KTimerEntity::new_with_timing(
                period_ticks,
                RtTiming::new(period_ticks, execution_ticks, execution_ticks),
            ),
            name,
        }
    }

    pub fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn execution_ticks(&self) -> u32 {
        self.entity.relative_deadline_ticks()
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn period_ticks(&self) -> u32 {
        self.entity.period_ticks()
    }
}

#[repr(C)]
pub(crate) struct WaitKTimer {
    pub(crate) entity: KTimerEntity,
    pub(crate) name: &'static str,
}

impl WaitKTimer {
    pub const fn new() -> Self {
        Self {
            entity: KTimerEntity {
                expire_at: KTIMER_EXPIRE_NEVER,
                node: RbNode::new(),
                active: false,
                timing: RtTiming::new(0, 0, 0),
            },
            name: "wait",
        }
    }

    pub fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }
}

#[repr(C)]
pub struct RtKTimer {
    pub(crate) entity: KTimerEntity,
    pub name: &'static str,
    pub miss_cnt: u32,
    thread_ctx: *mut ThreadCtx,
}

impl RtKTimer {
    pub const fn new(period_ticks: u32, thread_ctx: *mut ThreadCtx, name: &'static str) -> Self {
        Self::new_with_timing(RtTiming::from_period(period_ticks), thread_ctx, name)
    }

    pub const fn new_with_timing(
        timing: RtTiming,
        thread_ctx: *mut ThreadCtx,
        name: &'static str,
    ) -> Self {
        Self {
            entity: KTimerEntity::new_with_timing(timing.relative_deadline_ticks(), timing),
            name,
            miss_cnt: 0,
            thread_ctx,
        }
    }

    pub(crate) fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }

    pub(crate) fn thread_ctx(&self) -> *mut ThreadCtx {
        self.thread_ctx
    }

    pub fn timing(&self) -> RtTiming {
        self.entity.timing()
    }

    pub fn period_ticks(&self) -> u32 {
        self.entity.period_ticks()
    }

    pub fn relative_deadline_ticks(&self) -> u32 {
        self.entity.relative_deadline_ticks()
    }

    pub fn budget_ticks(&self) -> u32 {
        self.entity.budget_ticks()
    }

    pub(crate) fn init_rt_ktimer(&mut self, thread_ctx: *mut ThreadCtx) {
        self.thread_ctx = thread_ctx;
        if !thread_ctx.is_null() {
            unsafe {
                set_rt_ktimer_entity(ThreadHandle::from_thread_ctx(thread_ctx), self.entity_mut());
            }
        }
    }
}

/// Convert a raw tick interval into a scheduler timer reload register value.
///
/// scheduler timer stores a reload register value. A reload
/// value of `R` wraps after `R + 1` ticks, so an interval of `N` ticks must be
/// represented as `N - 1`. This helper returns the raw conversion and allows
/// reload `0`; scheduler programming raises the reload value `0` to
/// `SCHEDULER_TIMER_RELOAD_MIN` before writing the hardware register.
pub fn reload_from_ticks(ticks: u32) -> Option<u32> {
    ticks
        .checked_sub(1)
        .filter(|&reload| reload <= SCHEDULER_TIMER_RELOAD_MAX)
}

/// Initialize the global ktimer and wait-timer state.
///
/// # Safety
///
/// Call this during single-threaded scheduler setup, before interrupts or
/// threads can access the ktimer or wait queues. `init_cfs` and RT thread
/// spawning must happen after this initialization.
///
/// Do not call this while timer entities or wait entities are queued, running,
/// or otherwise visible to the scheduler. Reinitializing with live entities
/// invalidates their intrusive links and loses the current timer expiration.
pub unsafe fn init_ktimer_queue() {
    critical_section(|| unsafe {
        ptr::write(KTIMER_QUEUE.get(), KTimerQueue::new());
        ptr::write(&raw mut NEXT_KTIMER, ptr::null_mut());
        ptr::write(&raw mut WAIT_KTIMER, WaitKTimer::new());
        (*KTIMER_QUEUE.get()).enqueue_inactive((*ptr::addr_of_mut!(WAIT_KTIMER)).entity_mut());
    });
}

pub(crate) unsafe fn enqueue_ktimer(entity: *mut KTimerEntity) {
    critical_section(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        debug_assert!(
            !queue.contains(entity.cast_const()),
            "ktimer entity is already queued"
        );
        (*entity).reset_links();
        queue.enqueue_active(entity);
        refresh_next_ktimer(queue);
    });
}

unsafe fn update_wait_ktimer_deadline(wait_ktimer_entity: *mut KTimerEntity) {
    unsafe {
        let wait_entity = (*WAIT_QUEUE.get()).first();
        if wait_entity.is_null() {
            (*wait_ktimer_entity).set_expire_never();
        } else {
            (*wait_ktimer_entity).set_expire_at((*wait_entity).wake_at);
        }
    }
}

unsafe fn record_rt_budget_overrun(entity: *mut KTimerEntity, rt_thread: *mut RtThread) {
    unsafe {
        if (*rt_thread).runtime > (*entity).budget_ticks() {
            crate::rtsched_println!(
                "Budget overrun in thread '{}': runtime {} ticks exceeded budget {} ticks",
                (*rt_thread).thread.name,
                (*rt_thread).runtime,
                (*entity).budget_ticks()
            );
            let rt_ktimer = KTimerEntity::container_of(entity);
            (*rt_ktimer).miss_cnt = (*rt_ktimer).miss_cnt.saturating_add(1);
        }
    }
}

unsafe fn is_real_rt_deadline_miss(entity: *mut KTimerEntity, rt_thread: *mut RtThread) -> bool {
    unsafe { (*rt_thread).runtime > (*entity).relative_deadline_ticks() }
}

pub(crate) unsafe fn program_wait_ktimer() {
    critical_section(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        // Keep the linked node's raw pointers valid across the unchanged-deadline path.
        let wait_ktimer_entity = ptr::addr_of_mut!(WAIT_KTIMER.entity);

        let first_wait = (*WAIT_QUEUE.get()).first();
        let expire_at = if first_wait.is_null() {
            KTIMER_EXPIRE_NEVER
        } else {
            (*first_wait).wake_at
        };
        if (*wait_ktimer_entity).expire_at() != expire_at {
            queue.remove(wait_ktimer_entity);
            (*wait_ktimer_entity).set_expire_at(expire_at);
            queue.enqueue_inactive(wait_ktimer_entity);
        }
        // A wake can change the selected RT timer even when the wait deadline stays put.
        refresh_next_ktimer(queue);
    });
}

pub(crate) unsafe fn wake_wait_thread(queue: &mut KTimerQueue, elapsed: u32) {
    unsafe {
        loop {
            let Some(wait_thread) = pop_expired_wait_thread(queue.now_ticks()) else {
                break;
            };
            let wait_thread_ptr = wait_thread.as_ptr();

            (*wait_thread_ptr).set_state(ThreadState::Ready);
            crate::diagnostics::trace::record_wakeup(wait_thread_ptr);
            if (*wait_thread_ptr).is_cfs() {
                enqueue_runq_from_waitq(wait_thread);
            } else if (*wait_thread_ptr).is_rt() {
                let ktimer_entity = rt_ktimer_entity(wait_thread);
                let rt_thread = rt_thread_from_handle(wait_thread);

                (*rt_thread).runtime = (*rt_thread).runtime.saturating_add(elapsed);
                record_rt_budget_overrun(ktimer_entity, rt_thread);

                (*ktimer_entity).set_expire_after(
                    queue.now_ticks(),
                    (*ktimer_entity)
                        .relative_deadline_ticks()
                        .saturating_sub((*rt_thread).runtime),
                );
                (*ktimer_entity).reset_links();
                queue.enqueue_active(ktimer_entity);
            }
        }
    }
}

unsafe fn remove_ktimer(entity: *mut KTimerEntity) -> *mut KTimerEntity {
    critical_section(|| unsafe {
        if entity.is_null() {
            return ptr::null_mut();
        }

        let queue = &mut *KTIMER_QUEUE.get();
        let removed = queue.dequeue(entity);
        if NEXT_KTIMER == removed {
            refresh_next_ktimer(queue);
        }
        removed
    })
}

unsafe fn reinsert_ktimer(entity: *mut KTimerEntity) {
    critical_section(|| unsafe {
        if entity.is_null() {
            return;
        }

        let queue = &mut *KTIMER_QUEUE.get();
        debug_assert!(
            !queue.contains(entity.cast_const()),
            "ktimer entity is already queued"
        );
        (*entity).reset_links();
        queue.enqueue_active(entity);

        refresh_next_ktimer(queue);
    });
}

unsafe fn refresh_next_ktimer(queue: &mut KTimerQueue) {
    unsafe {
        let mut entity = queue.first_active();

        if !entity.is_null() && (*entity).is_expired_at(queue.now_ticks()) {
            queue.remove(entity);
            if is_cfs_ktimer(entity) {
                (*entity).set_expire_after(queue.now_ticks(), (*entity).period_ticks());
            } else {
                let rt_thread_ctx = (*KTimerEntity::container_of(entity)).thread_ctx();
                let rt_thread = rt_thread_from_handle(ThreadHandle::from_thread_ctx(rt_thread_ctx));

                if is_real_rt_deadline_miss(entity, rt_thread) {
                    crate::diagnostics::ktimer::record_rt_deadline_miss(queue, entity, rt_thread);
                }
                (*rt_thread).runtime = 0;
                (*entity).set_expire_after(queue.now_ticks(), (*entity).relative_deadline_ticks());
            }
            queue.enqueue_active(entity);

            entity = queue.first_active();
        }

        NEXT_KTIMER = entity;
    }
}

pub(crate) fn dequeue_ktimerq_to_waitq(thread: ThreadHandle) -> Result<(), WaitQueueError> {
    critical_section(|| unsafe {
        let thread_ptr = thread.as_ptr();
        if !(*thread_ptr).is_rt() {
            return Err(WaitQueueError::NotFound);
        }

        let ktimer_entity = rt_ktimer_entity(thread);
        if ktimer_entity.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        remove_ktimer(ktimer_entity);
        insert_wait_thread(thread);
        program_wait_ktimer();

        Ok(())
    })
}

pub(crate) fn enqueue_ktimerq_from_waitq(thread: ThreadHandle) -> Result<(), WaitQueueError> {
    critical_section(|| unsafe {
        let thread_ptr = thread.as_ptr();
        if !(*thread_ptr).is_rt() {
            return Err(WaitQueueError::NotFound);
        }

        let ktimer_entity = rt_ktimer_entity(thread);
        if ktimer_entity.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        remove_wait_thread(thread);

        (*thread_ptr).set_state(ThreadState::Ready);
        crate::diagnostics::trace::record_wakeup(thread_ptr);
        reinsert_ktimer(ktimer_entity);
        program_wait_ktimer();

        Ok(())
    })
}

pub fn next_ktimer_reload() -> Option<u32> {
    critical_section(|| unsafe {
        let queue = &*KTIMER_QUEUE.get();
        scheduler_timer_reload_for_entity(queue, queue.first())
    })
}

pub(crate) fn elapsed_ticks_since_last_interrupt() -> u32 {
    #[cfg(not(target_arch = "arm"))]
    {
        if TEST_ELAPSED_TICKS_OVERRIDE_SET.load(AtomicOrdering::Relaxed) {
            return TEST_ELAPSED_TICKS_OVERRIDE.load(AtomicOrdering::Relaxed);
        }
    }

    scheduler_timer_reload()
        .or_else(next_ktimer_reload)
        .map(|reload| reload.saturating_add(1))
        .unwrap_or_default()
}

#[cfg(not(target_arch = "arm"))]
pub(crate) fn set_elapsed_ticks_since_last_interrupt_for_test(elapsed: u32) {
    TEST_ELAPSED_TICKS_OVERRIDE.store(elapsed, AtomicOrdering::Relaxed);
    TEST_ELAPSED_TICKS_OVERRIDE_SET.store(true, AtomicOrdering::Relaxed);
}

#[cfg(not(target_arch = "arm"))]
pub(crate) fn clear_elapsed_ticks_since_last_interrupt_for_test() {
    TEST_ELAPSED_TICKS_OVERRIDE_SET.store(false, AtomicOrdering::Relaxed);
    TEST_ELAPSED_TICKS_OVERRIDE.store(0, AtomicOrdering::Relaxed);
}

pub(crate) fn elapsed_ticks_since_current_reload() -> u32 {
    elapsed_ticks_from_platform_timer()
}

fn scheduler_timer_reload() -> Option<u32> {
    platform::scheduler_timer_reload()
}

fn elapsed_ticks_from_platform_timer() -> u32 {
    match (
        platform::scheduler_timer_reload(),
        platform::scheduler_timer_current(),
    ) {
        (Some(reload), Some(current)) => reload.saturating_sub(current),
        _ => 0,
    }
}

pub(crate) unsafe fn advance_ktimers(elapsed: u32) {
    unsafe {
        (*KTIMER_QUEUE.get()).advance_time(elapsed);
    }
}

pub(crate) unsafe fn dispatch_expired_ktimer(elapsed: u32) -> *mut KTimerEntity {
    unsafe { (*KTIMER_QUEUE.get()).dispatch_expired(elapsed) }
}

pub(crate) unsafe fn update_next_ktimer(entity: *mut KTimerEntity) {
    critical_section(|| unsafe {
        NEXT_KTIMER = entity;
    });
}

unsafe fn thread_scheduler_ktimer(thread: ThreadHandle) -> *mut KTimerEntity {
    unsafe {
        match (*thread.as_ptr()).kind {
            ThreadKind::Cfs => ptr::addr_of_mut!(CFS_KTIMER.entity),
            ThreadKind::Rt => rt_ktimer_entity(thread),
            ThreadKind::Idle => ptr::null_mut(),
        }
    }
}

pub(crate) unsafe fn thread_scheduler_expire_at(thread: ThreadHandle) -> u64 {
    critical_section(|| unsafe {
        let entity = thread_scheduler_ktimer(thread);
        if entity.is_null() {
            KTIMER_EXPIRE_NEVER
        } else {
            (*entity).expire_at()
        }
    })
}

pub(crate) unsafe fn earliest_queued_scheduler_expire_at() -> u64 {
    critical_section(|| unsafe {
        let queue = &*KTIMER_QUEUE.get();
        let mut entity = queue.first();

        while !entity.is_null() {
            if !is_wait_ktimer(entity) {
                return (*entity).expire_at();
            }
            entity = queue.next(entity);
        }

        KTIMER_EXPIRE_NEVER
    })
}

pub(crate) unsafe fn set_thread_scheduler_expire_at(thread: ThreadHandle, expire_at: u64) {
    critical_section(|| unsafe {
        let entity = thread_scheduler_ktimer(thread);
        if entity.is_null() {
            return;
        }

        let queue = &mut *KTIMER_QUEUE.get();
        let was_queued = queue.contains(entity.cast_const());
        let was_active = (*entity).is_active();

        if was_queued {
            queue.remove(entity);
        }

        (*entity).set_expire_at(expire_at);

        if was_queued {
            (*entity).reset_links();
            if was_active {
                queue.enqueue_active(entity);
            } else {
                queue.enqueue_inactive(entity);
            }
            refresh_next_ktimer(queue);
        } else if NEXT_KTIMER == entity {
            refresh_next_ktimer(queue);
        }
    });
}

/// Return whether the named kernel timer is currently active.
///
/// Returns `false` when no timer with the given name exists.
pub fn is_active_ktimer(name: &str) -> bool {
    critical_section(|| unsafe {
        let queue = &*KTIMER_QUEUE.get();
        let mut entity = queue.first();

        while !entity.is_null() {
            if ktimer_name(entity) == name {
                return (*entity).is_active();
            }
            entity = queue.next(entity);
        }

        false
    })
}

pub(crate) fn next_ktimer() -> *mut KTimerEntity {
    critical_section(|| unsafe { NEXT_KTIMER })
}

pub(crate) fn with_ktimer_queue<R>(f: impl FnOnce(&KTimerQueue) -> R) -> R {
    critical_section(|| unsafe { f(&*KTIMER_QUEUE.get()) })
}

pub(crate) fn is_cfs_ktimer(entity: *const KTimerEntity) -> bool {
    !entity.is_null() && entity == unsafe { ptr::addr_of_mut!(CFS_KTIMER.entity).cast_const() }
}

pub(crate) fn is_wait_ktimer(entity: *const KTimerEntity) -> bool {
    !entity.is_null() && entity == unsafe { ptr::addr_of_mut!(WAIT_KTIMER.entity).cast_const() }
}

pub(crate) unsafe fn ktimer_name(entity: *const KTimerEntity) -> &'static str {
    unsafe {
        if is_cfs_ktimer(entity) {
            (*ptr::addr_of_mut!(CFS_KTIMER)).name
        } else if is_wait_ktimer(entity) {
            (*ptr::addr_of_mut!(WAIT_KTIMER)).name
        } else {
            (*KTimerEntity::container_of(entity.cast_mut())).name
        }
    }
}

pub(crate) unsafe fn yield_ktimer(
    entity: *mut KTimerEntity,
    elapsed: u32,
    reset_runtime: bool,
) -> *mut KTimerEntity {
    critical_section(|| unsafe {
        if entity.is_null() {
            return ptr::null_mut();
        }

        let queue: &mut KTimerQueue = &mut *KTIMER_QUEUE.get();

        queue.remove(entity);

        if is_cfs_ktimer(entity) {
            (*entity).set_expire_after(
                queue.now_ticks().saturating_add(u64::from(elapsed)),
                (*entity).period_ticks().saturating_sub(elapsed),
            );
        } else {
            let current_rt_thread_ctx = (*KTimerEntity::container_of(entity)).thread_ctx();
            let current_rt_thread =
                rt_thread_from_handle(ThreadHandle::from_thread_ctx(current_rt_thread_ctx));

            (*current_rt_thread).runtime = (*current_rt_thread).runtime.saturating_add(elapsed);
            record_rt_budget_overrun(entity, current_rt_thread);

            (*entity).set_expire_after(
                queue.now_ticks().saturating_add(u64::from(elapsed)),
                (*entity)
                    .period_ticks()
                    .saturating_sub((*current_rt_thread).runtime),
            );
            if reset_runtime {
                (*current_rt_thread).runtime = 0;
            }
        }

        queue.advance_time(elapsed);
        queue.enqueue_inactive(entity);
        queue.first_active()
    })
}

fn writable_reload(reload: u32) -> u32 {
    reload.clamp(SCHEDULER_TIMER_RELOAD_MIN, SCHEDULER_TIMER_RELOAD_MAX)
}

fn programmable_reload_from_ticks(ticks: u32) -> u32 {
    let reload = reload_from_ticks(ticks).unwrap_or(if ticks == 0 {
        0
    } else {
        SCHEDULER_TIMER_RELOAD_MAX
    });
    writable_reload(reload)
}

unsafe fn scheduler_timer_reload_for_entity(
    queue: &KTimerQueue,
    entity: *mut KTimerEntity,
) -> Option<u32> {
    if entity.is_null() {
        return None;
    }

    unsafe {
        let reload = if is_cfs_ktimer(entity) {
            let ticks = (*entity)
                .remaining_at(queue.now_ticks())
                .min((*entity).relative_deadline_ticks());
            programmable_reload_from_ticks(ticks)
        } else {
            let raw_reload = (*entity)
                .expire_at
                .saturating_sub(queue.now_ticks())
                .saturating_sub(1)
                .min(u64::from(SCHEDULER_TIMER_RELOAD_MAX)) as u32;
            writable_reload(raw_reload)
        };

        Some(reload)
    }
}

pub(crate) fn program_next_scheduler_timer() -> Option<u32> {
    critical_section(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        let reload = scheduler_timer_reload_for_entity(queue, queue.first())?;

        debug_assert!((SCHEDULER_TIMER_RELOAD_MIN..=SCHEDULER_TIMER_RELOAD_MAX).contains(&reload));
        let _ = platform::program_scheduler_timer_reload(reload);

        Some(reload)
    })
}

unsafe impl RBTreeNode for KTimerEntity {
    fn node(entity: *mut Self) -> *mut RbNode {
        if entity.is_null() {
            ptr::null_mut()
        } else {
            unsafe { ptr::addr_of_mut!((*entity).node) }
        }
    }

    fn entity_of(node: *mut RbNode) -> *mut Self {
        if node.is_null() {
            ptr::null_mut()
        } else {
            unsafe {
                (node as *mut u8)
                    .sub(offset_of!(KTimerEntity, node))
                    .cast::<KTimerEntity>()
            }
        }
    }

    fn entity_of_const(node: *const RbNode) -> *const Self {
        if node.is_null() {
            ptr::null()
        } else {
            unsafe {
                (node as *const u8)
                    .sub(offset_of!(KTimerEntity, node))
                    .cast::<KTimerEntity>()
            }
        }
    }

    unsafe fn cmp(a: *const Self, b: *const Self) -> core::cmp::Ordering {
        unsafe {
            match (*a).expire_at.cmp(&(*b).expire_at) {
                core::cmp::Ordering::Equal => (a as usize).cmp(&(b as usize)),
                other => other,
            }
        }
    }
}

pub struct KTimerQueue {
    active_tree: RBTree<KTimerEntity>,
    inactive_tree: RBTree<KTimerEntity>,
    first_active: *mut RbNode,
    first_inactive: *mut RbNode,
    now_ticks: u64,
}

impl KTimerQueue {
    pub const fn new() -> Self {
        Self {
            active_tree: RBTree::new(),
            inactive_tree: RBTree::new(),
            first_active: ptr::null_mut(),
            first_inactive: ptr::null_mut(),
            now_ticks: 0,
        }
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn is_empty(&self) -> bool {
        self.active_tree.is_empty() && self.inactive_tree.is_empty()
    }

    pub fn len(&self) -> usize {
        self.active_tree.len() + self.inactive_tree.len()
    }

    pub fn first(&self) -> *mut KTimerEntity {
        Self::earlier_of(
            self.first_active(),
            <KTimerEntity as RBTreeNode>::entity_of(self.first_inactive),
        )
    }

    pub fn first_active(&self) -> *mut KTimerEntity {
        <KTimerEntity as RBTreeNode>::entity_of(self.first_active)
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn last(&self) -> *mut KTimerEntity {
        let active = self.active_tree.last();
        let inactive = self.inactive_tree.last();
        if active.is_null() {
            inactive
        } else if inactive.is_null()
            || unsafe { <KTimerEntity as RBTreeNode>::cmp(active, inactive) } == Ordering::Greater
        {
            active
        } else {
            inactive
        }
    }

    /// Return the next timer in combined expiration/address order.
    pub fn next(&self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        if entity.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            let (own_tree, other_tree) = if (*entity).is_active() {
                (&self.active_tree, &self.inactive_tree)
            } else {
                (&self.inactive_tree, &self.active_tree)
            };
            Self::earlier_of(own_tree.next(entity), other_tree.upper_bound(entity))
        }
    }

    pub fn contains(&self, entity: *const KTimerEntity) -> bool {
        self.active_tree.contains(entity) || self.inactive_tree.contains(entity)
    }

    pub fn now_ticks(&self) -> u64 {
        self.now_ticks
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn next_deadline(&self) -> Option<u32> {
        let first = self.first();
        if first.is_null() {
            None
        } else {
            Some(unsafe { (*first).remaining_at(self.now_ticks) })
        }
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn next_reload(&self) -> Option<u32> {
        unsafe { scheduler_timer_reload_for_entity(self, self.first()) }
    }

    pub fn advance_time(&mut self, elapsed: u32) {
        self.now_ticks = self.now_ticks.saturating_add(u64::from(elapsed));
    }

    /// Dispatch all timers that expired at the queue's current time.
    ///
    /// # Safety
    ///
    /// The queue must contain only valid timer entities whose backing storage
    /// outlives the queue entry. Any RT timer entity in the queue must refer to
    /// a live `RtThread`, and the global wait queue must not be concurrently
    /// mutated while expired wait timers are processed.
    ///
    /// `elapsed` must be the elapsed tick count for the scheduler interval
    /// being dispatched. Callers must hold exclusive access to this queue and
    /// serialize dispatch against scheduler interrupts and other queue
    /// mutations.
    pub unsafe fn dispatch_expired(&mut self, elapsed: u32) -> *mut KTimerEntity {
        unsafe {
            loop {
                let expired = self.first();
                let first_active = self.first_active();
                if expired.is_null() || !(*expired).is_expired_at(self.now_ticks) {
                    return if first_active.is_null() {
                        ptr::null_mut()
                    } else {
                        first_active
                    };
                }

                self.remove(expired);

                if is_wait_ktimer(expired) {
                    wake_wait_thread(self, elapsed);
                    update_wait_ktimer_deadline(expired);
                    self.enqueue_inactive(expired);
                } else if is_cfs_ktimer(expired) {
                    if (*expired).is_active() {
                        (*expired).set_expire_after(
                            self.now_ticks,
                            (*expired).period_ticks().saturating_sub(elapsed),
                        );
                        self.enqueue_inactive(expired);
                    } else {
                        (*expired).set_expire_after(self.now_ticks, (*expired).period_ticks());
                        self.enqueue_active(expired);
                    }
                } else {
                    let thread_ctx = (*KTimerEntity::container_of(expired)).thread_ctx();
                    let rt_thread =
                        rt_thread_from_handle(ThreadHandle::from_thread_ctx(thread_ctx));
                    if (*expired).is_active() && is_real_rt_deadline_miss(expired, rt_thread) {
                        crate::diagnostics::ktimer::record_rt_deadline_miss(
                            self, expired, rt_thread,
                        );
                    }
                    (*rt_thread).runtime = 0;
                    (*expired)
                        .set_expire_after(self.now_ticks, (*expired).relative_deadline_ticks());
                    self.enqueue_active(expired);
                }
            }
        }
    }

    /// Enqueue a detached timer as eligible for scheduler selection.
    ///
    /// # Safety
    ///
    /// `entity` must be non-null, valid for mutation, and backed by storage
    /// that outlives its queue membership. It must not already be linked into
    /// this or any other timer queue. Callers must hold exclusive access to the
    /// queue and serialize insertion against scheduler interrupts and other
    /// queue mutations. Its expiration must remain unchanged until it is removed.
    pub unsafe fn enqueue_active(&mut self, entity: *mut KTimerEntity) {
        unsafe { self.insert(entity, true) };
    }

    /// Enqueue a detached timer as an inactive release or wakeup event.
    ///
    /// # Safety
    ///
    /// `entity` must be non-null, valid for mutation, and backed by storage
    /// that outlives its queue membership. It must not already be linked into
    /// this or any other timer queue. Callers must hold exclusive access to the
    /// queue and serialize insertion against scheduler interrupts and other
    /// queue mutations. Its expiration must remain unchanged until it is removed.
    pub unsafe fn enqueue_inactive(&mut self, entity: *mut KTimerEntity) {
        unsafe { self.insert(entity, false) };
    }

    unsafe fn insert(&mut self, entity: *mut KTimerEntity, active: bool) {
        unsafe {
            debug_assert!(
                !self.contains(entity.cast_const()),
                "entity is already linked into this tree"
            );
            let (tree, left_most) = if active {
                (&mut self.active_tree, &mut self.first_active)
            } else {
                (&mut self.inactive_tree, &mut self.first_inactive)
            };
            tree.insert(entity);
            (*entity).active = active;
            if left_most.is_null()
                || <KTimerEntity as RBTreeNode>::cmp(
                    entity.cast_const(),
                    <KTimerEntity as RBTreeNode>::entity_of_const(*left_most),
                ) == Ordering::Less
            {
                *left_most = <KTimerEntity as RBTreeNode>::node(entity);
            }
        }
    }

    /// Temporarily remove a timer, preserving its previous active state for
    /// expiry processing and deadline updates. Use `dequeue` when blocking it.
    ///
    /// # Safety
    ///
    /// `entity` must be non-null and currently linked into this queue. Callers
    /// must hold exclusive access to the queue and serialize removal against
    /// scheduler interrupts and other queue mutations. Its active state and
    /// expiration must still match the values used when it was inserted.
    pub unsafe fn remove(&mut self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        if entity.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            let (tree, left_most) = if (*entity).is_active() {
                (&mut self.active_tree, &mut self.first_active)
            } else {
                (&mut self.inactive_tree, &mut self.first_inactive)
            };
            let removing_left_most = ptr::eq(
                <KTimerEntity as RBTreeNode>::node(entity).cast_const(),
                (*left_most).cast_const(),
            );
            let next_left_most = if removing_left_most {
                <KTimerEntity as RBTreeNode>::node(tree.next(entity))
            } else {
                ptr::null_mut()
            };

            let removed = tree.remove(entity);
            if removing_left_most {
                *left_most = next_left_most;
            }

            removed
        }
    }

    /// Remove a timer and clear its scheduling eligibility, for example when
    /// its thread blocks. A null timer is returned unchanged.
    ///
    /// # Safety
    ///
    /// A non-null `entity` must satisfy the requirements of `Self::remove`.
    pub unsafe fn dequeue(&mut self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        unsafe {
            let removed = self.remove(entity);
            if !removed.is_null() {
                (*removed).active = false;
            }
            removed
        }
    }

    /// Remove and return the earliest ktimer entity in the queue.
    ///
    /// # Safety
    ///
    /// Every entity currently linked in the queue must still be valid for
    /// mutation and backed by storage that outlives the returned borrow.
    /// Callers must hold exclusive access to the queue and serialize removal
    /// against scheduler interrupts and other queue mutations.
    #[cfg(not(target_arch = "arm"))]
    pub unsafe fn pop_first(&mut self) -> Option<&mut KTimerEntity> {
        let first = self.first();
        if first.is_null() {
            return None;
        }

        unsafe {
            self.remove(first);
            Some(&mut *first)
        }
    }

    fn earlier_of(a: *mut KTimerEntity, b: *mut KTimerEntity) -> *mut KTimerEntity {
        if a.is_null() {
            return b;
        }
        if b.is_null() || unsafe { <KTimerEntity as RBTreeNode>::cmp(a, b) } == Ordering::Less {
            a
        } else {
            b
        }
    }
}

impl Default for KTimerQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "arm"))]
#[path = "../tests/support/ktimer.rs"]
pub mod test_support;
