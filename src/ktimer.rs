// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Kernel timer queue keyed by ktimer deadline.
//!
//! The queue is intrusive: each `KTimerEntity` embeds its own `RbNode`, so
//! inserting ktimers does not allocate.

use core::cell::UnsafeCell;
use core::cmp::Ordering;
use core::mem::offset_of;
use core::ptr;

use crate::arch::platform;
use crate::arch::platform::{SCHEDULER_TIMER_RELOAD_MAX, SCHEDULER_TIMER_RELOAD_MIN};
use crate::critical_section;
use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::runq::enqueue_runq_from_waitq;
use crate::sync::SyncType;
use crate::thread::{
    CfsThread, RtThread, ThreadCtx, ThreadHandle, ThreadRef, ThreadState, cfs_thread_from_handle,
    rt_ktimer_entity, rt_thread_from_handle, set_rt_ktimer_entity,
};
use crate::waitq::{
    WAIT_QUEUE, WaitQueueError, insert_wait_thread, pop_expired_wait_thread, remove_wait_thread,
};

const KTIMER_DEADLINE_NEVER: u64 = u64::MAX;
static KTIMER_QUEUE: GlobalKTimerQueue = GlobalKTimerQueue::new();
static mut NEXT_KTIMER: *mut KTimerEntity = ptr::null_mut();
pub(crate) static mut CFS_KTIMER: CfsKTimer = CfsKTimer::new(0, 0, "cfs");
pub(crate) static mut WAIT_KTIMER: WaitKTimer = WaitKTimer::inactive();

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
    deadline_at: u64,
    node: RbNode,
    active: bool,
    pub miss_cnt: u32,
    timing: RtTiming,
}

impl KTimerEntity {
    #[allow(dead_code)]
    pub const fn new(deadline_ticks: u32) -> Self {
        Self::new_with_timing(deadline_ticks, RtTiming::from_period(deadline_ticks))
    }

    pub const fn new_with_timing(deadline_ticks: u32, timing: RtTiming) -> Self {
        Self {
            deadline_at: deadline_ticks as u64,
            node: RbNode::new(),
            active: true,
            miss_cnt: 0,
            timing,
        }
    }

    #[allow(dead_code)]
    pub fn deadline(&self) -> u32 {
        self.deadline_at.min(u64::from(u32::MAX)) as u32
    }

    pub fn set_deadline(&mut self, deadline: u32) {
        self.deadline_at = u64::from(deadline);
    }

    #[allow(dead_code)]
    pub fn deadline_at(&self) -> u64 {
        self.deadline_at
    }

    pub fn set_deadline_at(&mut self, deadline_at: u64) {
        self.deadline_at = deadline_at;
    }

    pub fn set_deadline_after(&mut self, now_ticks: u64, ticks: u32) {
        self.deadline_at = now_ticks.saturating_add(u64::from(ticks));
    }

    pub fn set_deadline_never(&mut self) {
        self.deadline_at = KTIMER_DEADLINE_NEVER;
    }

    pub fn remaining_at(&self, now_ticks: u64) -> u32 {
        if self.deadline_at == KTIMER_DEADLINE_NEVER {
            return SCHEDULER_TIMER_RELOAD_MAX;
        }

        self.deadline_at
            .saturating_sub(now_ticks)
            .min(u64::from(SCHEDULER_TIMER_RELOAD_MAX)) as u32
    }

    pub fn is_expired_at(&self, now_ticks: u64) -> bool {
        self.deadline_at <= now_ticks
    }

    pub fn reset_links(&mut self) {
        self.node.reset_links();
    }

    #[allow(dead_code)]
    pub fn is_linked(&self) -> bool {
        self.node.is_linked()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
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
    pub unsafe fn rt_ktimer(entity: *mut Self) -> *mut RtKTimer {
        debug_assert!(!entity.is_null());
        debug_assert!(!is_cfs_ktimer(entity));

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

    #[allow(dead_code)]
    pub fn execution_ticks(&self) -> u32 {
        self.entity.relative_deadline_ticks()
    }

    #[allow(dead_code)]
    pub fn period_ticks(&self) -> u32 {
        self.entity.period_ticks()
    }

    #[allow(dead_code)]
    pub fn timing(&self) -> RtTiming {
        self.entity.timing()
    }
}

#[repr(C)]
pub(crate) struct WaitKTimer {
    pub(crate) entity: KTimerEntity,
    pub(crate) name: &'static str,
}

impl WaitKTimer {
    pub const fn inactive() -> Self {
        Self {
            entity: KTimerEntity {
                deadline_at: KTIMER_DEADLINE_NEVER,
                node: RbNode::new(),
                active: false,
                miss_cnt: 0,
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

    pub fn init_rt_thread(&mut self, thread: &mut RtThread) {
        self.init_rt_ktimer(thread.thread_ctx_mut());
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
/// invalidates their intrusive links and loses the current timer deadline.
pub unsafe fn init_ktimer_queue() {
    critical_section(|| unsafe {
        ptr::write(KTIMER_QUEUE.get(), KTimerQueue::new());
        ptr::write(&raw mut NEXT_KTIMER, ptr::null_mut());
        ptr::write(&raw mut WAIT_KTIMER, WaitKTimer::inactive());
        (*KTIMER_QUEUE.get()).insert((*ptr::addr_of_mut!(WAIT_KTIMER)).entity_mut());
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
        queue.insert(entity);
        refresh_next_ktimer(queue);
    });
}

unsafe fn update_wait_ktimer_deadline(wait_ktimer_entity: *mut KTimerEntity) {
    unsafe {
        let wait_entity = (*WAIT_QUEUE.get()).first();
        if wait_entity.is_null() {
            (*wait_ktimer_entity).set_deadline_never();
        } else {
            (*wait_ktimer_entity).set_deadline_at((*wait_entity).wake_at);
        }
        (*wait_ktimer_entity).set_active(false);
    }
}

unsafe fn cfs_period_ticks(entity: *mut KTimerEntity) -> u32 {
    unsafe { (*entity).period_ticks() }
}

unsafe fn cfs_execution_ticks(entity: *mut KTimerEntity) -> u32 {
    unsafe { (*entity).relative_deadline_ticks() }
}

unsafe fn rt_period_ticks(entity: *mut KTimerEntity) -> u32 {
    unsafe { (*entity).period_ticks() }
}

unsafe fn rt_relative_deadline_ticks(entity: *mut KTimerEntity) -> u32 {
    unsafe { (*entity).relative_deadline_ticks() }
}

unsafe fn rt_budget_ticks(entity: *mut KTimerEntity) -> u32 {
    unsafe { (*entity).budget_ticks() }
}

unsafe fn set_rt_active_deadline_after_runtime(
    entity: *mut KTimerEntity,
    now_ticks: u64,
    runtime: u32,
) {
    unsafe {
        (*entity).set_deadline_after(
            now_ticks,
            rt_relative_deadline_ticks(entity).saturating_sub(runtime),
        );
    }
}

unsafe fn set_rt_next_release_after_runtime(
    entity: *mut KTimerEntity,
    now_ticks: u64,
    runtime: u32,
) {
    unsafe {
        (*entity).set_deadline_after(now_ticks, rt_period_ticks(entity).saturating_sub(runtime));
    }
}

unsafe fn set_rt_next_deadline(entity: *mut KTimerEntity, now_ticks: u64) {
    unsafe {
        (*entity).set_deadline_after(now_ticks, rt_relative_deadline_ticks(entity));
    }
}

unsafe fn record_rt_budget_overrun(entity: *mut KTimerEntity, rt_thread: *mut RtThread) {
    unsafe {
        if (*rt_thread).runtime > rt_budget_ticks(entity) {
            crate::rtsched_println!(
                "Budget overrun in thread '{}': runtime {} ticks exceeded budget {} ticks",
                (*rt_thread).thread.name,
                (*rt_thread).runtime,
                rt_budget_ticks(entity)
            );
            (*entity).miss_cnt = (*entity).miss_cnt.saturating_add(1);
        }
    }
}

unsafe fn is_real_rt_deadline_miss(entity: *mut KTimerEntity, rt_thread: *mut RtThread) -> bool {
    unsafe { (*rt_thread).runtime > rt_relative_deadline_ticks(entity) }
}

unsafe fn record_rt_deadline_miss(
    queue: &KTimerQueue,
    entity: *mut KTimerEntity,
    rt_thread: *mut RtThread,
) {
    unsafe {
        let thread_name = (*rt_thread).thread.name;
        let runtime = (*rt_thread).runtime;
        let relative_deadline = rt_relative_deadline_ticks(entity);

        crate::trace::record_deadline_miss(
            ptr::addr_of!((*rt_thread).thread),
            runtime,
            relative_deadline,
        );
        (*entity).miss_cnt = (*entity).miss_cnt.saturating_add(1);
        crate::rtsched_println!(
            "Deadline miss in thread '{}': timer expired at relative deadline {} ticks (runtime {} ticks)",
            thread_name,
            relative_deadline,
            runtime
        );
        print_rt_deadline_miss_diagnostics(queue, entity, rt_thread);
        panic!("RT deadline miss in thread '{}'", thread_name);
    }
}

unsafe fn print_rt_deadline_miss_diagnostics(
    queue: &KTimerQueue,
    missed_entity: *mut KTimerEntity,
    missed_thread: *mut RtThread,
) {
    unsafe {
        crate::rtsched_println!("rt deadline miss diagnostics:");
        print_ktimer_queue_statistics(queue, missed_entity);
        print_thread_statistics(queue, missed_entity, missed_thread);
    }
}

unsafe fn print_ktimer_queue_statistics(queue: &KTimerQueue, missed_entity: *mut KTimerEntity) {
    unsafe {
        crate::rtsched_println!(
            "ktimer queue statistics: now={} len={} first_active='{}'",
            queue.now_ticks(),
            queue.len(),
            ktimer_name_or_none(queue.first_active())
        );

        let mut entity = queue.first();
        let mut printed_missed = false;
        while !entity.is_null() {
            let is_missed = ptr::eq(entity.cast_const(), missed_entity.cast_const());
            if is_missed {
                printed_missed = true;
            }
            print_ktimer_statistics(queue, entity, if is_missed { "*" } else { " " }, true);
            entity = queue.next(entity);
        }

        if !missed_entity.is_null() && !printed_missed {
            crate::rtsched_println!("  * expired timer was already removed from the queue");
            print_ktimer_statistics(queue, missed_entity, "*", false);
        }
    }
}

unsafe fn print_ktimer_statistics(
    queue: &KTimerQueue,
    entity: *mut KTimerEntity,
    marker: &'static str,
    queued: bool,
) {
    unsafe {
        crate::rtsched_println!(
            "  {} name='{}' kind={} queued={} deadline_at={} remaining={} active={} misses={}",
            marker,
            ktimer_name(entity),
            ktimer_kind_name(entity),
            yes_no(queued),
            (*entity).deadline_at(),
            (*entity).remaining_at(queue.now_ticks()),
            yes_no((*entity).is_active()),
            (*entity).miss_cnt
        );

        if !is_cfs_ktimer(entity) && !is_wait_ktimer(entity) {
            let rt_ktimer = KTimerEntity::rt_ktimer(entity);
            let thread_ctx = (*rt_ktimer).thread_ctx();
            if thread_ctx.is_null() {
                crate::rtsched_println!(
                    "    rt timing: period={} relative_deadline={} budget={} thread=<none>",
                    (*entity).period_ticks(),
                    (*entity).relative_deadline_ticks(),
                    (*entity).budget_ticks()
                );
            } else {
                let rt_thread = rt_thread_from_handle(ThreadHandle::from_thread_ctx(thread_ctx));
                crate::rtsched_println!(
                    "    rt timing: period={} relative_deadline={} budget={} thread_id={} thread_state={} runtime={}",
                    (*entity).period_ticks(),
                    (*entity).relative_deadline_ticks(),
                    (*entity).budget_ticks(),
                    (*thread_ctx).id,
                    thread_state_name((*thread_ctx).state),
                    (*rt_thread).runtime
                );
            }
        }
    }
}

unsafe fn print_thread_statistics(
    queue: &KTimerQueue,
    missed_entity: *mut KTimerEntity,
    missed_thread: *mut RtThread,
) {
    unsafe {
        crate::rtsched_println!("thread statistics:");
        print_rt_thread_statistics(
            "  ",
            "missed",
            &*missed_thread,
            missed_entity,
            queue.now_ticks(),
        );
        print_current_thread_statistics(queue.now_ticks());
        print_idle_thread_statistics();
        print_cfs_thread_statistics_list();
        print_wait_thread_statistics_list();
    }
}

unsafe fn print_current_thread_statistics(now_ticks: u64) {
    unsafe {
        let current = crate::sched::CURRENT_THREAD_CTX;
        if current.is_null() {
            crate::rtsched_println!("  current: <none>");
            return;
        }

        let current = ThreadHandle::from_thread_ctx(current);
        if crate::sched::CURRENT_THREAD_IS_CFS {
            print_cfs_thread_statistics("  ", "current", &*cfs_thread_from_handle(current));
        } else {
            print_rt_thread_statistics(
                "  ",
                "current",
                &*rt_thread_from_handle(current),
                rt_ktimer_entity(current),
                now_ticks,
            );
        }
    }
}

unsafe fn print_idle_thread_statistics() {
    unsafe {
        let idle = crate::sched::IDLE_THREAD_CTX;
        if idle.is_null() {
            crate::rtsched_println!("  idle: <none>");
            return;
        }

        let idle = ThreadHandle::from_thread_ctx(idle);
        print_cfs_thread_statistics("  ", "idle", &*cfs_thread_from_handle(idle));
    }
}

fn print_cfs_thread_statistics_list() {
    crate::rtsched_println!("  cfs threads:");
    let mut saw_thread = false;
    crate::runq::traverse_run_queue_fn(|thread| {
        saw_thread = true;
        print_cfs_thread_statistics("    ", "cfs", thread);
    });
    if !saw_thread {
        crate::rtsched_println!("    <empty>");
    }
}

fn print_wait_thread_statistics_list() {
    crate::rtsched_println!("  wait queue:");
    let mut saw_thread = false;
    crate::waitq::traverse_wait_queue_fn(|thread| {
        saw_thread = true;
        match thread {
            ThreadRef::Cfs(thread) => {
                let (wait_ticks, waitevt) = thread.wait_info();
                print_wait_thread_statistics(
                    "    ",
                    "wait",
                    thread.thread_ctx(),
                    "cfs",
                    wait_ticks,
                    waitevt,
                    None,
                );
            }
            ThreadRef::Rt(thread) => {
                let (wait_ticks, waitevt) = thread.wait_info();
                print_wait_thread_statistics(
                    "    ",
                    "wait",
                    thread.thread_ctx(),
                    "rt",
                    wait_ticks,
                    waitevt,
                    Some(thread.runtime()),
                );
            }
        }
    });
    if !saw_thread {
        crate::rtsched_println!("    <empty>");
    }
}

fn print_cfs_thread_statistics(indent: &str, label: &str, thread: &CfsThread) {
    let thread_ctx = thread.thread_ctx();
    let sched_info = thread.sched_info();
    crate::rtsched_println!(
        "{}{}: id={} name='{}' class=cfs state={} priority={} sched_ticks={} vruntime={}",
        indent,
        label,
        thread_ctx.id,
        thread_ctx.name,
        thread_state_name(thread_ctx.state),
        sched_info.priority,
        sched_info.sched_tick_cnt,
        sched_info.vruntime
    );
}

fn print_rt_thread_statistics(
    indent: &str,
    label: &str,
    thread: &RtThread,
    ktimer_entity: *mut KTimerEntity,
    now_ticks: u64,
) {
    let thread_ctx = thread.thread_ctx();
    if ktimer_entity.is_null() {
        crate::rtsched_println!(
            "{}{}: id={} name='{}' class=rt state={} runtime={} ktimer=<none>",
            indent,
            label,
            thread_ctx.id,
            thread_ctx.name,
            thread_state_name(thread_ctx.state),
            thread.runtime()
        );
        return;
    }

    unsafe {
        crate::rtsched_println!(
            "{}{}: id={} name='{}' class=rt state={} runtime={} ktimer='{}' deadline_at={} remaining={} active={} misses={}",
            indent,
            label,
            thread_ctx.id,
            thread_ctx.name,
            thread_state_name(thread_ctx.state),
            thread.runtime(),
            ktimer_name(ktimer_entity),
            (*ktimer_entity).deadline_at(),
            (*ktimer_entity).remaining_at(now_ticks),
            yes_no((*ktimer_entity).is_active()),
            (*ktimer_entity).miss_cnt
        );
    }
}

fn print_wait_thread_statistics(
    indent: &str,
    label: &str,
    thread_ctx: &ThreadCtx,
    class: &'static str,
    wait_ticks: u32,
    waitevt: Option<SyncType>,
    runtime: Option<u32>,
) {
    if let Some(runtime) = runtime {
        crate::rtsched_println!(
            "{}{}: id={} name='{}' class={} state={} runtime={} wait_ticks={} waitevt={}",
            indent,
            label,
            thread_ctx.id,
            thread_ctx.name,
            class,
            thread_state_name(thread_ctx.state),
            runtime,
            wait_ticks,
            sync_type_name(waitevt)
        );
    } else {
        crate::rtsched_println!(
            "{}{}: id={} name='{}' class={} state={} wait_ticks={} waitevt={}",
            indent,
            label,
            thread_ctx.id,
            thread_ctx.name,
            class,
            thread_state_name(thread_ctx.state),
            wait_ticks,
            sync_type_name(waitevt)
        );
    }
}

pub(crate) unsafe fn program_wait_ktimer() {
    critical_section(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        let wait_ktimer = ptr::addr_of_mut!(WAIT_KTIMER);
        let wait_ktimer_entity = (*wait_ktimer).entity_mut();

        queue.remove(wait_ktimer_entity);
        update_wait_ktimer_deadline(wait_ktimer_entity);
        queue.insert(wait_ktimer_entity);
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
            crate::trace::record_wakeup(wait_thread_ptr);
            if (*wait_thread_ptr).is_cfs {
                enqueue_runq_from_waitq(wait_thread);
            } else {
                let ktimer_entity = rt_ktimer_entity(wait_thread);
                let rt_thread = rt_thread_from_handle(wait_thread);

                (*rt_thread).runtime = (*rt_thread).runtime.saturating_add(elapsed);
                record_rt_budget_overrun(ktimer_entity, rt_thread);

                (*ktimer_entity).set_active(true);
                set_rt_active_deadline_after_runtime(
                    ktimer_entity,
                    queue.now_ticks(),
                    (*rt_thread).runtime,
                );
                (*ktimer_entity).reset_links();
                queue.insert(ktimer_entity);
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
        let removed = queue.remove(entity);
        (*removed).set_active(false);
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
        (*entity).set_active(true);
        (*entity).reset_links();
        queue.insert(entity);

        refresh_next_ktimer(queue);
    });
}

unsafe fn normalize_next_ktimer(
    queue: &mut KTimerQueue,
    mut entity: *mut KTimerEntity,
) -> *mut KTimerEntity {
    unsafe {
        if entity.is_null() {
            entity = activate_cfs_ktimer();
        }

        if !entity.is_null() && (*entity).is_expired_at(queue.now_ticks()) {
            queue.remove(entity);
            if is_cfs_ktimer(entity) {
                (*entity).miss_cnt = (*entity).miss_cnt.saturating_add(1);
                (*entity).set_deadline_after(queue.now_ticks(), cfs_period_ticks(entity));
            } else {
                let rt_thread_ctx = (*KTimerEntity::rt_ktimer(entity)).thread_ctx();
                let rt_thread = rt_thread_from_handle(ThreadHandle::from_thread_ctx(rt_thread_ctx));

                if is_real_rt_deadline_miss(entity, rt_thread) {
                    record_rt_deadline_miss(queue, entity, rt_thread);
                }
                (*rt_thread).runtime = 0;
                set_rt_next_deadline(entity, queue.now_ticks());
            }
            queue.insert(entity);

            entity = queue.first_active();
            if entity.is_null() {
                entity = activate_cfs_ktimer();
            }
        }

        entity
    }
}

unsafe fn refresh_next_ktimer(queue: &mut KTimerQueue) {
    unsafe {
        NEXT_KTIMER = normalize_next_ktimer(queue, queue.first_active());
    }
}

pub(crate) fn dequeue_ktimerq_to_waitq(thread: ThreadHandle) -> Result<(), WaitQueueError> {
    critical_section(|| unsafe {
        let thread_ptr = thread.as_ptr();
        if (*thread_ptr).is_cfs {
            return Err(WaitQueueError::NotFound);
        }

        let ktimer_entity = rt_ktimer_entity(thread);
        if ktimer_entity.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        remove_ktimer(ktimer_entity);
        (*thread_ptr).set_state(ThreadState::Waiting);

        insert_wait_thread(thread);
        program_wait_ktimer();

        Ok(())
    })
}

pub fn dequeue_rt_thread_to_waitq(thread: &mut RtThread) -> Result<(), WaitQueueError> {
    unsafe { dequeue_ktimerq_to_waitq(ThreadHandle::from_thread_ctx(thread.thread_ctx_mut())) }
}

pub(crate) fn enqueue_ktimerq_from_waitq(thread: ThreadHandle) -> Result<(), WaitQueueError> {
    critical_section(|| unsafe {
        let thread_ptr = thread.as_ptr();
        if (*thread_ptr).is_cfs {
            return Err(WaitQueueError::NotFound);
        }

        let ktimer_entity = rt_ktimer_entity(thread);
        if ktimer_entity.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        remove_wait_thread(thread);

        (*thread_ptr).set_state(ThreadState::Ready);
        crate::trace::record_wakeup(thread_ptr);
        reinsert_ktimer(ktimer_entity);
        program_wait_ktimer();

        Ok(())
    })
}

pub fn enqueue_rt_thread_from_waitq(thread: &mut RtThread) -> Result<(), WaitQueueError> {
    unsafe { enqueue_ktimerq_from_waitq(ThreadHandle::from_thread_ctx(thread.thread_ctx_mut())) }
}

pub fn next_ktimer_reload() -> Option<u32> {
    critical_section(|| unsafe { (*KTIMER_QUEUE.get()).next_reload() })
}

pub(crate) fn elapsed_ticks_since_last_interrupt() -> u32 {
    scheduler_timer_reload()
        .or_else(next_ktimer_reload)
        .map(|reload| reload.saturating_add(1))
        .unwrap_or_default()
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
        if (*thread.as_ptr()).is_cfs {
            ptr::addr_of_mut!(CFS_KTIMER.entity)
        } else {
            rt_ktimer_entity(thread)
        }
    }
}

pub(crate) unsafe fn thread_scheduler_deadline_at(thread: ThreadHandle) -> u64 {
    critical_section(|| unsafe {
        let entity = thread_scheduler_ktimer(thread);
        if entity.is_null() {
            KTIMER_DEADLINE_NEVER
        } else {
            (*entity).deadline_at()
        }
    })
}

pub(crate) unsafe fn earliest_queued_scheduler_deadline_at() -> u64 {
    critical_section(|| unsafe {
        let queue = &*KTIMER_QUEUE.get();
        let mut entity = queue.first();

        while !entity.is_null() {
            if !is_wait_ktimer(entity) {
                return (*entity).deadline_at();
            }
            entity = queue.next(entity);
        }

        KTIMER_DEADLINE_NEVER
    })
}

pub(crate) unsafe fn set_thread_scheduler_deadline_at(thread: ThreadHandle, deadline_at: u64) {
    critical_section(|| unsafe {
        let entity = thread_scheduler_ktimer(thread);
        if entity.is_null() {
            return;
        }

        let queue = &mut *KTIMER_QUEUE.get();
        let was_queued = queue.contains(entity.cast_const());

        if was_queued {
            queue.remove(entity);
        }

        (*entity).set_deadline_at(deadline_at);

        if was_queued {
            (*entity).reset_links();
            queue.insert(entity);
            refresh_next_ktimer(queue);
        } else if NEXT_KTIMER == entity {
            refresh_next_ktimer(queue);
        }
    });
}

pub fn traverse_ktimer_queue() {
    critical_section(|| unsafe {
        let queue = &*KTIMER_QUEUE.get();
        let mut entity = queue.first();

        crate::rtsched_println!("ktimer queue:");
        while !entity.is_null() {
            crate::rtsched_println!(
                "{} ktimer's deadline={}, active={}",
                ktimer_name(entity),
                (*entity).remaining_at(queue.now_ticks()),
                (*entity).is_active()
            );
            entity = queue.next(entity);
        }
    });
}

/// Traverse the ktimer queue and invoke `f` for each ktimer with its name
/// and deadline. This is similar to `traverse_ktimer_queue` but allows the
/// caller to handle formatting/output (for example, writing to UART).
pub fn traverse_ktimer_queue_fn<F>(mut f: F)
where
    F: FnMut(&'static str, u32),
{
    critical_section(|| unsafe {
        let queue = &*KTIMER_QUEUE.get();
        let mut entity = queue.first();

        while !entity.is_null() {
            f(
                ktimer_name(entity),
                (*entity).remaining_at(queue.now_ticks()),
            );
            entity = queue.next(entity);
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

pub(crate) fn ktimer_now_ticks() -> u64 {
    critical_section(|| unsafe { (*KTIMER_QUEUE.get()).now_ticks() })
}

pub(crate) fn is_cfs_ktimer(entity: *const KTimerEntity) -> bool {
    !entity.is_null() && entity == cfs_ktimer().cast_const()
}

pub(crate) fn is_wait_ktimer(entity: *const KTimerEntity) -> bool {
    !entity.is_null() && entity == wait_ktimer().cast_const()
}

fn cfs_ktimer() -> *mut KTimerEntity {
    unsafe { ptr::addr_of_mut!(CFS_KTIMER.entity) }
}

fn wait_ktimer() -> *mut KTimerEntity {
    unsafe { ptr::addr_of_mut!(WAIT_KTIMER.entity) }
}

#[allow(dead_code)]
unsafe fn ktimer_name(entity: *const KTimerEntity) -> &'static str {
    unsafe {
        if is_cfs_ktimer(entity) {
            (*ptr::addr_of_mut!(CFS_KTIMER)).name
        } else if is_wait_ktimer(entity) {
            (*ptr::addr_of_mut!(WAIT_KTIMER)).name
        } else {
            (*KTimerEntity::rt_ktimer(entity.cast_mut())).name
        }
    }
}

unsafe fn ktimer_name_or_none(entity: *const KTimerEntity) -> &'static str {
    if entity.is_null() {
        "<none>"
    } else {
        unsafe { ktimer_name(entity) }
    }
}

fn ktimer_kind_name(entity: *const KTimerEntity) -> &'static str {
    if is_cfs_ktimer(entity) {
        "cfs"
    } else if is_wait_ktimer(entity) {
        "wait"
    } else {
        "rt"
    }
}

fn thread_state_name(state: ThreadState) -> &'static str {
    match state {
        ThreadState::Ready => "Ready",
        ThreadState::Running => "Running",
        ThreadState::Waiting => "Waiting",
    }
}

fn sync_type_name(waitevt: Option<SyncType>) -> &'static str {
    match waitevt {
        Some(SyncType::BinarySemaphore) => "binary-semaphore",
        Some(SyncType::CountingSemaphore) => "counting-semaphore",
        Some(SyncType::Mutex) => "mutex",
        None => "none",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

unsafe fn activate_cfs_ktimer() -> *mut KTimerEntity {
    let cfs = cfs_ktimer();
    if !cfs.is_null() {
        unsafe {
            (*cfs).set_active(true);
        }
    }
    cfs
}

pub(crate) unsafe fn yield_ktimer(
    entity: *mut KTimerEntity,
    elapsed: u32,
    reset_runtime: bool,
) -> *mut KTimerEntity {
    critical_section(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        yield_ktimer_in_queue(queue, entity, elapsed, reset_runtime)
    })
}

unsafe fn yield_ktimer_in_queue(
    queue: &mut KTimerQueue,
    entity: *mut KTimerEntity,
    elapsed: u32,
    reset_runtime: bool,
) -> *mut KTimerEntity {
    unsafe {
        if entity.is_null() {
            return ptr::null_mut();
        }

        queue.remove(entity);

        if is_cfs_ktimer(entity) {
            (*entity).set_deadline_after(
                queue.now_ticks().saturating_add(u64::from(elapsed)),
                cfs_period_ticks(entity).saturating_sub(elapsed),
            );
        } else {
            let current_rt_thread_ctx = (*KTimerEntity::rt_ktimer(entity)).thread_ctx();
            let current_rt_thread =
                rt_thread_from_handle(ThreadHandle::from_thread_ctx(current_rt_thread_ctx));

            (*current_rt_thread).runtime = (*current_rt_thread).runtime.saturating_add(elapsed);
            record_rt_budget_overrun(entity, current_rt_thread);

            set_rt_next_release_after_runtime(
                entity,
                queue.now_ticks().saturating_add(u64::from(elapsed)),
                (*current_rt_thread).runtime,
            );
            if reset_runtime {
                (*current_rt_thread).runtime = 0;
            }
        }

        (*entity).set_active(false);
        queue.advance_time(elapsed);
        queue.insert(entity);
        let next = queue.first_active();
        if next.is_null() {
            activate_cfs_ktimer()
        } else {
            next
        }
    }
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
                .min(cfs_execution_ticks(entity));
            programmable_reload_from_ticks(ticks)
        } else {
            let raw_reload = (*entity)
                .deadline_at
                .saturating_sub(queue.now_ticks())
                .saturating_sub(1)
                .min(u64::from(SCHEDULER_TIMER_RELOAD_MAX)) as u32;
            writable_reload(raw_reload)
        };

        Some(reload)
    }
}

unsafe fn next_scheduler_timer_reload(queue: &KTimerQueue) -> Option<u32> {
    unsafe { scheduler_timer_reload_for_entity(queue, queue.first()) }
}

pub(crate) fn program_next_scheduler_timer() -> Option<u32> {
    critical_section(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        let reload = next_scheduler_timer_reload(queue)?;

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
            match (*a).deadline_at.cmp(&(*b).deadline_at) {
                core::cmp::Ordering::Equal => (a as usize).cmp(&(b as usize)),
                other => other,
            }
        }
    }
}

pub struct KTimerQueue {
    tree: RBTree<KTimerEntity>,
    left_most: *mut RbNode,
    now_ticks: u64,
}

impl KTimerQueue {
    pub const fn new() -> Self {
        Self {
            tree: RBTree::new(),
            left_most: ptr::null_mut(),
            now_ticks: 0,
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    #[allow(dead_code)]
    pub fn root(&self) -> *mut KTimerEntity {
        self.tree.root()
    }

    pub fn first(&self) -> *mut KTimerEntity {
        <KTimerEntity as RBTreeNode>::entity_of(self.left_most)
    }

    #[allow(dead_code)]
    pub fn last(&self) -> *mut KTimerEntity {
        self.tree.last()
    }

    pub fn next(&self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        self.tree.next(entity)
    }

    pub fn contains(&self, entity: *const KTimerEntity) -> bool {
        self.tree.contains(entity)
    }

    pub fn now_ticks(&self) -> u64 {
        self.now_ticks
    }

    #[allow(dead_code)]
    pub fn next_deadline(&self) -> Option<u32> {
        let first = self.first();
        if first.is_null() {
            None
        } else {
            Some(unsafe { (*first).remaining_at(self.now_ticks) })
        }
    }

    pub fn next_reload(&self) -> Option<u32> {
        unsafe { next_scheduler_timer_reload(self) }
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
                let (expired, first_active) = self.first_and_first_active();
                if expired.is_null() || !(*expired).is_expired_at(self.now_ticks) {
                    return if first_active.is_null() {
                        activate_cfs_ktimer()
                    } else {
                        first_active
                    };
                }

                self.remove(expired);

                if is_wait_ktimer(expired) {
                    wake_wait_thread(self, elapsed);
                    update_wait_ktimer_deadline(expired);
                    self.insert(expired);
                } else if is_cfs_ktimer(expired) {
                    if (*expired).is_active() {
                        (*expired).set_deadline_after(
                            self.now_ticks,
                            cfs_period_ticks(expired).saturating_sub(elapsed),
                        );
                        (*expired).set_active(false);
                        self.insert(expired);
                    } else {
                        (*expired).set_deadline_after(self.now_ticks, cfs_period_ticks(expired));
                        (*expired).set_active(true);
                        self.insert(expired);
                    }
                } else {
                    let thread_ctx = (*KTimerEntity::rt_ktimer(expired)).thread_ctx();
                    let rt_thread =
                        rt_thread_from_handle(ThreadHandle::from_thread_ctx(thread_ctx));
                    if (*expired).is_active() && is_real_rt_deadline_miss(expired, rt_thread) {
                        record_rt_deadline_miss(self, expired, rt_thread);
                    }
                    (*rt_thread).runtime = 0;
                    set_rt_next_deadline(expired, self.now_ticks);
                    (*expired).set_active(true);
                    self.insert(expired);
                }
            }
        }
    }

    /// Insert a detached ktimer entity into the queue.
    ///
    /// # Safety
    ///
    /// `entity` must be non-null, valid for mutation, and backed by storage
    /// that outlives its queue membership. It must not already be linked into
    /// this or any other timer queue. Callers must hold exclusive access to the
    /// queue and serialize insertion against scheduler interrupts and other
    /// queue mutations.
    pub unsafe fn insert(&mut self, entity: *mut KTimerEntity) {
        unsafe {
            self.tree.insert(entity);
            if self.left_most.is_null()
                || <KTimerEntity as RBTreeNode>::cmp(
                    entity.cast_const(),
                    <KTimerEntity as RBTreeNode>::entity_of_const(self.left_most),
                ) == Ordering::Less
            {
                self.left_most = <KTimerEntity as RBTreeNode>::node(entity);
            }
        }
    }

    /// Remove a ktimer entity from the queue.
    ///
    /// # Safety
    ///
    /// `entity` must be non-null and currently linked into this queue. Callers
    /// must hold exclusive access to the queue and serialize removal against
    /// scheduler interrupts and other queue mutations.
    pub unsafe fn remove(&mut self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        unsafe {
            let removing_left_most = !entity.is_null()
                && ptr::eq(
                    <KTimerEntity as RBTreeNode>::node(entity).cast_const(),
                    self.left_most.cast_const(),
                );
            let next_left_most = if removing_left_most {
                <KTimerEntity as RBTreeNode>::node(self.tree.next(entity))
            } else {
                ptr::null_mut()
            };

            let removed = self.tree.remove(entity);
            if removing_left_most {
                self.left_most = next_left_most;
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
    #[allow(dead_code)]
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

    fn first_and_first_active(&self) -> (*mut KTimerEntity, *mut KTimerEntity) {
        let first = self.first();
        let mut entity = first;

        while !entity.is_null() {
            if unsafe { (*entity).is_active() } {
                return (first, entity);
            }
            entity = self.next(entity);
        }

        (first, ptr::null_mut())
    }

    pub fn first_active(&self) -> *mut KTimerEntity {
        self.first_and_first_active().1
    }
}

impl Default for KTimerQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::thread::{RtThread, ThreadCtx, ThreadHandle, ThreadState};
    use crate::waitq::{WAIT_QUEUE, WaitEntity, insert_wait_thread, wait_entity};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::string::String;
    use std::sync::Mutex;
    use std::vec::Vec;

    static DEADLINE_MISS_PRINTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn capture_deadline_miss_print(message: &str) {
        DEADLINE_MISS_PRINTED
            .lock()
            .unwrap()
            .push(String::from(message));
    }

    fn discard_print(_message: &str) {}

    fn rt_thread(name: &'static str) -> RtThread {
        RtThread {
            thread: ThreadCtx {
                sp: 0,
                exc_return: 0,
                id: 1,
                name,
                state: ThreadState::Ready,
                is_cfs: false,
            },
            wait_entity: WaitEntity::new(),
            sync_entity: crate::sync::SyncEntity::new(),
            ktimer_entity: ptr::null_mut(),
            runtime: 0,
        }
    }

    unsafe fn thread_handle(thread: *mut ThreadCtx) -> ThreadHandle {
        unsafe { ThreadHandle::from_thread_ctx(thread) }
    }

    fn reset_wait_queue() {
        unsafe {
            *WAIT_QUEUE.get() = RBTree::new();
        }
    }

    fn collect_deadlines_at(queue: &KTimerQueue) -> Vec<u64> {
        let mut deadlines = Vec::new();
        let mut entity = queue.first();

        while !entity.is_null() {
            unsafe {
                deadlines.push((*entity).deadline_at());
            }
            entity = queue.next(entity);
        }

        deadlines
    }

    fn collect_remaining(queue: &KTimerQueue) -> Vec<u32> {
        let mut remaining = Vec::new();
        let mut entity = queue.first();

        while !entity.is_null() {
            unsafe {
                remaining.push((*entity).remaining_at(queue.now_ticks()));
            }
            entity = queue.next(entity);
        }

        remaining
    }

    fn collect_active_deadlines_at(queue: &KTimerQueue) -> Vec<u64> {
        let mut deadlines = Vec::new();
        let mut entity = queue.first();

        while !entity.is_null() {
            unsafe {
                if (*entity).is_active() {
                    deadlines.push((*entity).deadline_at());
                }
            }
            entity = queue.next(entity);
        }

        deadlines
    }

    #[test]
    fn reload_from_ticks_converts_to_scheduler_timer_reload() {
        assert_eq!(reload_from_ticks(0), None);
        assert_eq!(reload_from_ticks(1), Some(0));
        assert_eq!(reload_from_ticks(2), Some(SCHEDULER_TIMER_RELOAD_MIN));
        assert_eq!(reload_from_ticks(42), Some(41));
        assert_eq!(
            reload_from_ticks(SCHEDULER_TIMER_RELOAD_MAX + 1),
            Some(SCHEDULER_TIMER_RELOAD_MAX)
        );
        assert_eq!(reload_from_ticks(SCHEDULER_TIMER_RELOAD_MAX + 2), None);
    }

    #[test]
    fn writable_reload_clamps_raw_zero_to_minimum_reload() {
        assert_eq!(writable_reload(0), SCHEDULER_TIMER_RELOAD_MIN);
        assert_eq!(
            writable_reload(SCHEDULER_TIMER_RELOAD_MAX + 1),
            SCHEDULER_TIMER_RELOAD_MAX
        );
        assert_eq!(
            programmable_reload_from_ticks(0),
            SCHEDULER_TIMER_RELOAD_MIN
        );
        assert_eq!(
            programmable_reload_from_ticks(1),
            SCHEDULER_TIMER_RELOAD_MIN
        );
    }

    #[test]
    fn insert_orders_timers_by_deadline() {
        let mut queue = KTimerQueue::new();
        let mut timers = [
            KTimerEntity::new(30),
            KTimerEntity::new(10),
            KTimerEntity::new(20),
            KTimerEntity::new(5),
        ];

        for timer in &mut timers {
            unsafe {
                queue.insert(timer);
            }
        }

        assert_eq!(queue.len(), timers.len());
        assert_eq!(collect_deadlines_at(&queue), [5, 10, 20, 30]);
        assert_eq!(collect_remaining(&queue), [5, 10, 20, 30]);
        assert_eq!(queue.next_deadline(), Some(5));
        assert_eq!(queue.next_reload(), Some(4));
        unsafe {
            assert_eq!((*queue.first()).deadline_at(), 5);
            assert_eq!((*queue.last()).deadline_at(), 30);
        }
    }

    #[test]
    fn equal_deadlines_keep_strict_entity_ordering() {
        let mut queue = KTimerQueue::new();
        let mut first = KTimerEntity::new(12);
        let mut second = KTimerEntity::new(12);

        unsafe {
            queue.insert(&mut first);
            queue.insert(&mut second);
        }

        assert_eq!(queue.len(), 2);
        assert_eq!(collect_deadlines_at(&queue), [12, 12]);
    }

    #[test]
    #[should_panic(expected = "entity is already linked into this tree")]
    fn ktimer_queue_rejects_duplicate_timer_insertions() {
        let mut queue = KTimerQueue::new();
        let mut timer = KTimerEntity::new(12);

        unsafe {
            queue.insert(&mut timer);
            queue.insert(&mut timer);
        }
    }

    #[test]
    fn advance_time_updates_queue_clock_without_rewriting_deadlines() {
        let mut queue = KTimerQueue::new();
        let mut short = KTimerEntity::new(3);
        let mut long = KTimerEntity::new(20);
        let mut parked = WaitKTimer::inactive();

        unsafe {
            queue.insert(&mut short);
            queue.insert(&mut long);
            queue.insert(parked.entity_mut());
        }
        queue.advance_time(5);

        assert_eq!(queue.now_ticks(), 5);
        assert_eq!(short.deadline_at(), 3);
        assert_eq!(long.deadline_at(), 20);
        assert_eq!(parked.entity.deadline_at(), KTIMER_DEADLINE_NEVER);
        assert_eq!(short.remaining_at(queue.now_ticks()), 0);
        assert_eq!(long.remaining_at(queue.now_ticks()), 15);
        assert_eq!(
            parked.entity.remaining_at(queue.now_ticks()),
            SCHEDULER_TIMER_RELOAD_MAX
        );
        assert_eq!(
            collect_remaining(&queue),
            [0, 15, SCHEDULER_TIMER_RELOAD_MAX]
        );
    }

    #[test]
    fn next_reload_clamps_immediate_deadlines_to_minimum_reload() {
        let mut queue = KTimerQueue::new();
        let mut expired = KTimerEntity::new(0);

        unsafe {
            queue.insert(&mut expired);
        }

        assert_eq!(queue.next_deadline(), Some(0));
        assert_eq!(queue.next_reload(), Some(SCHEDULER_TIMER_RELOAD_MIN));
    }

    #[test]
    fn next_reload_clamps_far_deadlines_to_maximum_reload() {
        let mut queue = KTimerQueue::new();
        let mut far = KTimerEntity::new(1);
        far.set_deadline_at(u64::from(SCHEDULER_TIMER_RELOAD_MAX) + 42);

        unsafe {
            queue.insert(&mut far);
        }

        assert_eq!(queue.next_deadline(), Some(SCHEDULER_TIMER_RELOAD_MAX));
        assert_eq!(queue.next_reload(), Some(SCHEDULER_TIMER_RELOAD_MAX));
    }

    #[test]
    fn cfs_reload_honors_boosted_deadline_before_execution_slice() {
        let _guard = TEST_LOCK.lock().unwrap();

        unsafe {
            init_ktimer_queue();
            crate::sched::init_cfs(100, 25);

            let queue = &mut *KTIMER_QUEUE.get();
            let cfs = ptr::addr_of_mut!(CFS_KTIMER.entity);

            queue.remove(cfs);
            (*cfs).set_deadline_at(5);
            (*cfs).reset_links();
            queue.insert(cfs);

            assert_eq!(queue.next_reload(), Some(4));
        }
    }

    #[test]
    fn long_rt_deadline_is_dispatched_after_multiple_scheduler_timer_chunks() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");
        let max_chunk_ticks = SCHEDULER_TIMER_RELOAD_MAX + 1;
        let long_deadline = u64::from(max_chunk_ticks) * 2 + 5;

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            ktimer.entity.set_deadline_at(long_deadline);
            queue.insert(ktimer.entity_mut());
        }

        assert_eq!(queue.next_reload(), Some(SCHEDULER_TIMER_RELOAD_MAX));

        queue.advance_time(max_chunk_ticks);
        let next = unsafe { queue.dispatch_expired(max_chunk_ticks) };
        assert!(ptr::eq(next, ktimer.entity_mut()));
        assert_eq!(queue.now_ticks(), u64::from(max_chunk_ticks));
        assert_eq!(ktimer.entity.deadline_at(), long_deadline);
        assert_eq!(ktimer.entity.miss_cnt, 0);
        assert_eq!(queue.next_reload(), Some(SCHEDULER_TIMER_RELOAD_MAX));

        queue.advance_time(max_chunk_ticks);
        let next = unsafe { queue.dispatch_expired(max_chunk_ticks) };
        assert!(ptr::eq(next, ktimer.entity_mut()));
        assert_eq!(queue.now_ticks(), u64::from(max_chunk_ticks) * 2);
        assert_eq!(ktimer.entity.deadline_at(), long_deadline);
        assert_eq!(ktimer.entity.miss_cnt, 0);
        assert_eq!(queue.next_reload(), Some(4));

        queue.advance_time(5);
        let next = unsafe { queue.dispatch_expired(5) };

        assert!(ptr::eq(next, ktimer.entity_mut()));
        assert_eq!(queue.now_ticks(), long_deadline);
        assert_eq!(ktimer.entity.miss_cnt, 0);
        assert_eq!(ktimer.entity.deadline_at(), long_deadline + 50);
    }

    #[test]
    fn first_active_skips_inactive_timers() {
        let mut queue = KTimerQueue::new();
        let mut inactive_early = KTimerEntity::new(5);
        let mut active_late = KTimerEntity::new(20);
        let mut inactive_middle = KTimerEntity::new(10);

        inactive_early.set_active(false);
        inactive_middle.set_active(false);

        unsafe {
            queue.insert(&mut active_late);
            queue.insert(&mut inactive_early);
            queue.insert(&mut inactive_middle);
        }

        assert!(ptr::eq(queue.first_active(), &active_late));
    }

    #[test]
    fn active_timers_remain_sorted_by_absolute_deadline() {
        let mut queue = KTimerQueue::new();
        let mut active_middle = KTimerEntity::new(30);
        let mut inactive_first = KTimerEntity::new(5);
        let mut active_first = KTimerEntity::new(10);
        let mut inactive_late = KTimerEntity::new(40);
        let mut active_last = KTimerEntity::new(50);

        inactive_first.set_active(false);
        inactive_late.set_active(false);

        unsafe {
            queue.insert(&mut active_middle);
            queue.insert(&mut inactive_first);
            queue.insert(&mut active_first);
            queue.insert(&mut inactive_late);
            queue.insert(&mut active_last);
        }

        assert_eq!(collect_deadlines_at(&queue), [5, 10, 30, 40, 50]);
        assert_eq!(collect_active_deadlines_at(&queue), [10, 30, 50]);
        assert!(ptr::eq(queue.first_active(), &active_first));
    }

    #[test]
    fn remove_detaches_timer_from_queue() {
        let mut queue = KTimerQueue::new();
        let mut timers = [
            KTimerEntity::new(8),
            KTimerEntity::new(4),
            KTimerEntity::new(12),
        ];

        for timer in &mut timers {
            unsafe {
                queue.insert(timer);
            }
        }

        let removed = unsafe { queue.remove(&mut timers[1]) };

        assert!(ptr::eq(removed, &timers[1]));
        assert!(!timers[1].is_linked());
        assert_eq!(queue.len(), 2);
        assert_eq!(collect_deadlines_at(&queue), [8, 12]);
    }

    #[test]
    fn left_most_cache_tracks_queue_mutations() {
        let mut queue = KTimerQueue::new();
        let mut late = KTimerEntity::new(30);
        let mut early = KTimerEntity::new(5);
        let mut middle = KTimerEntity::new(20);

        assert!(queue.left_most.is_null());

        unsafe {
            queue.insert(&mut late);
        }
        assert!(ptr::eq(queue.first(), &late));

        unsafe {
            queue.insert(&mut early);
        }
        assert!(ptr::eq(queue.first(), &early));

        unsafe {
            queue.insert(&mut middle);
        }
        assert!(ptr::eq(queue.first(), &early));

        unsafe {
            queue.remove(&mut early);
        }
        assert!(ptr::eq(queue.first(), &middle));

        let popped = unsafe { queue.pop_first() }.unwrap() as *mut KTimerEntity;
        assert!(ptr::eq(popped, ptr::addr_of_mut!(middle)));
        assert!(ptr::eq(queue.first(), &late));

        unsafe {
            queue.remove(&mut late);
        }
        assert!(queue.left_most.is_null());
        assert!(queue.first().is_null());
    }

    #[test]
    fn pop_first_returns_timers_in_deadline_order() {
        let mut queue = KTimerQueue::new();
        let mut timers = [
            KTimerEntity::new(40),
            KTimerEntity::new(15),
            KTimerEntity::new(25),
            KTimerEntity::new(1),
        ];

        for timer in &mut timers {
            unsafe {
                queue.insert(timer);
            }
        }

        let mut popped = Vec::new();
        while let Some(timer) = unsafe { queue.pop_first() } {
            popped.push(timer.deadline_at());
            assert!(!timer.is_linked());
        }

        assert_eq!(popped, [1, 15, 25, 40]);
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.next_deadline(), None);
        assert_eq!(queue.next_reload(), None);
    }

    #[test]
    fn rt_timing_constructor_separates_period_deadline_and_budget() {
        let ktimer = RtKTimer::new_with_timing(RtTiming::new(100, 40, 20), ptr::null_mut(), "rt");

        assert_eq!(ktimer.period_ticks(), 100);
        assert_eq!(ktimer.relative_deadline_ticks(), 40);
        assert_eq!(ktimer.budget_ticks(), 20);
        assert_eq!(ktimer.timing(), RtTiming::new(100, 40, 20));
        assert_eq!(ktimer.entity.timing(), RtTiming::new(100, 40, 20));
        assert_eq!(ktimer.entity.deadline_at(), 40);
    }

    #[test]
    fn rt_yield_marks_timer_inactive_and_preserves_remaining_period() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer = RtKTimer::new(100, ptr::null_mut(), "rt");
        let mut active_later = KTimerEntity::new(200);

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            queue.insert(ktimer.entity_mut());
            queue.insert(&mut active_later);

            let next = yield_ktimer_in_queue(&mut queue, ktimer.entity_mut(), 20, false);
            assert!(ptr::eq(next, &active_later));
        }

        assert_eq!(rt.runtime, 20);
        assert!(!ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 100);
        assert_eq!(queue.now_ticks(), 20);
        assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 80);
    }

    #[test]
    fn rt_yield_with_reset_runtime_finishes_current_job_window() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer = RtKTimer::new(60, ptr::null_mut(), "rt");
        let mut active_later = KTimerEntity::new(120);

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            queue.insert(ktimer.entity_mut());
            queue.insert(&mut active_later);

            let next = yield_ktimer_in_queue(&mut queue, ktimer.entity_mut(), 15, true);
            assert!(ptr::eq(next, &active_later));
        }

        assert_eq!(rt.runtime, 0);
        assert!(!ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 60);
        assert_eq!(queue.now_ticks(), 15);
        assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 45);
    }

    #[test]
    fn rt_yield_uses_period_for_next_release_not_relative_deadline() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer =
            RtKTimer::new_with_timing(RtTiming::new(100, 40, 100), ptr::null_mut(), "rt");
        let mut active_later = KTimerEntity::new(200);

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            queue.insert(ktimer.entity_mut());
            queue.insert(&mut active_later);

            let next = yield_ktimer_in_queue(&mut queue, ktimer.entity_mut(), 15, false);
            assert!(ptr::eq(next, &active_later));
        }

        assert_eq!(rt.runtime, 15);
        assert!(!ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 100);
        assert_eq!(queue.now_ticks(), 15);
        assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 85);
        assert_eq!(ktimer.entity.miss_cnt, 0);
    }

    #[test]
    fn rt_yield_records_budget_overrun_independent_of_deadline() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer =
            RtKTimer::new_with_timing(RtTiming::new(100, 80, 10), ptr::null_mut(), "rt");
        let mut active_later = KTimerEntity::new(200);

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            queue.insert(ktimer.entity_mut());
            queue.insert(&mut active_later);

            let next = yield_ktimer_in_queue(&mut queue, ktimer.entity_mut(), 15, false);
            assert!(ptr::eq(next, &active_later));
        }

        assert_eq!(rt.runtime, 15);
        assert_eq!(ktimer.entity.miss_cnt, 1);
        assert!(!ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 100);
    }

    #[test]
    fn wake_wait_thread_charges_wait_elapsed_to_rt_deadline() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_wait_queue();
        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer = RtKTimer::new(100, ptr::null_mut(), "rt");

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            rt.runtime = 20;
            rt.thread.state = ThreadState::Waiting;
            rt.wait_entity.set_wake_after(0, 50);
            insert_wait_thread(thread_handle(&mut rt.thread));
        }

        queue.advance_time(50);

        unsafe {
            wake_wait_thread(&mut queue, 30);
        }

        assert!(rt.thread.state == ThreadState::Ready);
        assert_eq!(rt.runtime, 50);
        assert!(ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 100);
        assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 50);
        assert!(ptr::eq(queue.first_active(), ktimer.entity_mut()));
    }

    #[test]
    fn wake_wait_thread_uses_relative_deadline_not_period() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_wait_queue();
        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer =
            RtKTimer::new_with_timing(RtTiming::new(100, 60, 100), ptr::null_mut(), "rt");

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            rt.runtime = 20;
            rt.thread.state = ThreadState::Waiting;
            rt.wait_entity.set_wake_after(0, 50);
            insert_wait_thread(thread_handle(&mut rt.thread));
        }

        queue.advance_time(50);

        unsafe {
            wake_wait_thread(&mut queue, 30);
        }

        assert_eq!(rt.runtime, 50);
        assert!(ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 60);
        assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 10);
    }

    #[test]
    fn rt_waiting_thread_is_moved_from_ktimer_queue_to_wait_queue() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_wait_queue();
        let mut rt = rt_thread("rt");
        let mut ktimer = RtKTimer::new(100, ptr::null_mut(), "rt");
        rt.wait_entity.set_wake_after(0, 10);

        unsafe {
            init_ktimer_queue();
            crate::sched::init_cfs(1_000, 25);
            ktimer.init_rt_thread(&mut rt);
            enqueue_ktimer(ktimer.entity_mut());

            assert!((*KTIMER_QUEUE.get()).contains(ktimer.entity_mut()));

            assert!(dequeue_rt_thread_to_waitq(&mut rt).is_ok());

            assert!(rt.thread.state == ThreadState::Waiting);
            assert!(!(*KTIMER_QUEUE.get()).contains(ktimer.entity_mut()));
            let handle = thread_handle(&mut rt.thread);
            assert!((*WAIT_QUEUE.get()).contains(wait_entity(handle)));
            assert!(ptr::eq(rt_ktimer_entity(handle), ktimer.entity_mut()));

            assert!(enqueue_rt_thread_from_waitq(&mut rt).is_ok());

            assert!(rt.thread.state == ThreadState::Ready);
            assert!(
                (*KTIMER_QUEUE.get()).contains(ktimer.entity_mut()),
                "deadlines after enqueue from waitq: {:?}",
                collect_deadlines_at(&*KTIMER_QUEUE.get())
            );
            assert!(!(*WAIT_QUEUE.get()).contains(wait_entity(handle)));
        }
    }

    #[test]
    fn dispatch_expired_active_rt_timer_records_deadline_miss() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            rt.runtime = 55;
            ktimer.entity.set_deadline_at(10);
            queue.insert(ktimer.entity_mut());
        }

        queue.advance_time(10);
        DEADLINE_MISS_PRINTED.lock().unwrap().clear();
        crate::set_print_fn(capture_deadline_miss_print);
        let result = catch_unwind(AssertUnwindSafe(|| unsafe {
            queue.dispatch_expired(10);
        }));
        crate::set_print_fn(discard_print);

        assert!(result.is_err());
        let printed = DEADLINE_MISS_PRINTED.lock().unwrap();
        assert!(
            printed
                .iter()
                .any(|message| message.contains("ktimer queue statistics"))
        );
        assert!(
            printed
                .iter()
                .any(|message| message.contains("thread statistics"))
        );
        assert_eq!(ktimer.entity.miss_cnt, 1);
        assert_eq!(rt.runtime, 55);
        assert!(ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 10);
    }

    #[test]
    fn dispatch_expired_active_rt_timer_uses_relative_deadline() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer =
            RtKTimer::new_with_timing(RtTiming::new(100, 40, 20), ptr::null_mut(), "rt");

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            rt.runtime = 45;
            ktimer.entity.set_deadline_at(100);
            queue.insert(ktimer.entity_mut());
        }

        queue.advance_time(100);
        let result = catch_unwind(AssertUnwindSafe(|| unsafe {
            queue.dispatch_expired(100);
        }));

        assert!(result.is_err());
        assert_eq!(ktimer.entity.miss_cnt, 1);
        assert_eq!(rt.runtime, 45);
        assert!(ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 100);
    }

    #[test]
    fn dispatch_expired_active_rt_timer_without_runtime_over_deadline_is_not_miss() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            rt.runtime = 50;
            ktimer.entity.set_deadline_at(10);
            queue.insert(ktimer.entity_mut());
        }

        queue.advance_time(10);
        let next = unsafe { queue.dispatch_expired(10) };

        assert!(ptr::eq(next, ktimer.entity_mut()));
        assert_eq!(ktimer.entity.miss_cnt, 0);
        assert_eq!(rt.runtime, 0);
        assert!(ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 60);
    }

    #[test]
    fn dispatch_expired_inactive_rt_timer_reactivates_without_miss() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            rt.runtime = 30;
            ktimer.entity.set_active(false);
            ktimer.entity.set_deadline_at(10);
            queue.insert(ktimer.entity_mut());
        }

        queue.advance_time(10);
        let next = unsafe { queue.dispatch_expired(10) };

        assert!(ptr::eq(next, ktimer.entity_mut()));
        assert_eq!(ktimer.entity.miss_cnt, 0);
        assert_eq!(rt.runtime, 0);
        assert!(ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 60);
    }

    #[test]
    fn dispatch_expired_inactive_rt_timer_uses_relative_deadline() {
        let _guard = TEST_LOCK.lock().unwrap();

        let mut queue = KTimerQueue::new();
        let mut rt = rt_thread("rt");
        let mut ktimer =
            RtKTimer::new_with_timing(RtTiming::new(100, 40, 20), ptr::null_mut(), "rt");

        unsafe {
            ktimer.init_rt_ktimer(&mut rt.thread);
            rt.runtime = 30;
            ktimer.entity.set_active(false);
            ktimer.entity.set_deadline_at(100);
            queue.insert(ktimer.entity_mut());
        }

        queue.advance_time(100);
        let next = unsafe { queue.dispatch_expired(100) };

        assert!(ptr::eq(next, ktimer.entity_mut()));
        assert_eq!(ktimer.entity.miss_cnt, 0);
        assert_eq!(rt.runtime, 0);
        assert!(ktimer.entity.is_active());
        assert_eq!(ktimer.entity.deadline_at(), 140);
    }
}
