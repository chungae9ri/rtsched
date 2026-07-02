// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Kernel timer queue keyed by ktimer deadline.
//!
//! The queue is intrusive: each `KTimerEntity` embeds its own `RbNode`, so
//! inserting ktimers does not allocate.

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

use cortex_m::peripheral::SYST;

use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::runq::enqueue_runq_from_waitq;
use crate::thread::{
    ThreadCtx, ThreadState, rt_ktimer_entity, rt_thread_from_thread_ctx, set_rt_ktimer_entity,
};
use crate::waitq::{
    WAIT_QUEUE, WaitQueueError, insert_wait_thread, pop_expired_wait_thread, remove_wait_thread,
};

pub const CM_SYSTICK_RELOAD_BITS: u32 = 24;
pub const CM_SYSTICK_RELOAD_MAX: u32 = (1 << CM_SYSTICK_RELOAD_BITS) - 1;
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

#[cfg(target_arch = "arm")]
fn ktimer_critical<R>(f: impl FnOnce() -> R) -> R {
    cortex_m::interrupt::free(|_| f())
}

#[cfg(not(target_arch = "arm"))]
fn ktimer_critical<R>(f: impl FnOnce() -> R) -> R {
    f()
}

#[repr(C)]
pub struct KTimerEntity {
    duration: KTimerDuration,
    deadline_at: u64,
    node: RbNode,
    active: bool,
    pub miss_cnt: u32,
}

/// KTimerDuration is immutable, shouldn't be changed after initialization.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KTimerDuration(u32);

impl KTimerDuration {
    const fn from_ticks(ticks: u32) -> Self {
        Self(ticks)
    }

    const fn ticks(self) -> u32 {
        self.0
    }
}

impl KTimerEntity {
    pub const fn new(duration: u32) -> Self {
        Self {
            duration: KTimerDuration::from_ticks(duration),
            deadline_at: duration as u64,
            node: RbNode::new(),
            active: true,
            miss_cnt: 0,
        }
    }

    pub fn duration(&self) -> u32 {
        self.duration.ticks()
    }

    pub fn deadline(&self) -> u32 {
        self.deadline_at.min(u64::from(u32::MAX)) as u32
    }

    pub fn set_deadline(&mut self, deadline: u32) {
        self.deadline_at = u64::from(deadline);
    }

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
            return CM_SYSTICK_RELOAD_MAX;
        }

        self.deadline_at
            .saturating_sub(now_ticks)
            .min(u64::from(CM_SYSTICK_RELOAD_MAX)) as u32
    }

    pub fn is_expired_at(&self, now_ticks: u64) -> bool {
        self.deadline_at <= now_ticks
    }

    pub fn reset_links(&mut self) {
        self.node.reset_links();
    }

    pub fn is_linked(&self) -> bool {
        self.node.is_linked()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    pub unsafe fn rt_ktimer(entity: *mut Self) -> *mut RtKTimer {
        debug_assert!(!entity.is_null());
        debug_assert!(!is_cfs_ktimer(entity));

        (entity as *mut u8)
            .wrapping_sub(offset_of!(RtKTimer, entity))
            .cast::<RtKTimer>()
    }

    pub unsafe fn cfs_ktimer(entity: *mut Self) -> *mut CfsKTimer {
        debug_assert!(!entity.is_null());
        debug_assert!(is_cfs_ktimer(entity));

        (entity as *mut u8)
            .wrapping_sub(offset_of!(CfsKTimer, entity))
            .cast::<CfsKTimer>()
    }
}

#[repr(C)]
pub struct CfsKTimer {
    pub entity: KTimerEntity,
    pub name: &'static str,
    pub execution_ticks: u32,
}

impl CfsKTimer {
    pub const fn new(duration: u32, execution_ticks: u32, name: &'static str) -> Self {
        Self {
            entity: KTimerEntity::new(duration),
            name,
            execution_ticks,
        }
    }

    pub fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }

    pub fn execution_ticks(&self) -> u32 {
        self.execution_ticks
    }
}

#[repr(C)]
pub struct WaitKTimer {
    pub entity: KTimerEntity,
    pub name: &'static str,
}

impl WaitKTimer {
    pub const fn new(duration: u32, name: &'static str) -> Self {
        Self {
            entity: KTimerEntity::new(duration),
            name,
        }
    }

    pub const fn inactive() -> Self {
        Self {
            entity: KTimerEntity {
                duration: KTimerDuration::from_ticks(CM_SYSTICK_RELOAD_MAX),
                deadline_at: KTIMER_DEADLINE_NEVER,
                node: RbNode::new(),
                active: false,
                miss_cnt: 0,
            },
            name: "wait",
        }
    }

    pub fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }

    pub fn reset_links(&mut self) {
        self.entity.reset_links();
    }
}

#[repr(C)]
pub struct RtKTimer {
    pub entity: KTimerEntity,
    pub name: &'static str,
    thread_ctx: *mut ThreadCtx,
}

impl RtKTimer {
    pub const fn new(duration: u32, thread_ctx: *mut ThreadCtx, name: &'static str) -> Self {
        Self {
            entity: KTimerEntity::new(duration),
            name,
            thread_ctx,
        }
    }

    pub fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }

    pub fn thread_ctx(&self) -> *mut ThreadCtx {
        self.thread_ctx
    }

    pub fn init_rt_ktimer(&mut self, thread_ctx: *mut ThreadCtx) {
        self.thread_ctx = thread_ctx;
        if !thread_ctx.is_null() {
            unsafe {
                set_rt_ktimer_entity(thread_ctx, self.entity_mut());
            }
        }
    }
}

/// Convert a raw tick interval into a SysTick reload register value.
///
/// SysTick reload stores `ticks - 1`, and the register is 24 bits wide.
pub fn reload_from_ticks(ticks: u32) -> Option<u32> {
    ticks
        .checked_sub(1)
        .filter(|&reload| reload <= CM_SYSTICK_RELOAD_MAX)
}

pub unsafe fn init_ktimer_queue() {
    ktimer_critical(|| unsafe {
        ptr::write(KTIMER_QUEUE.get(), KTimerQueue::new());
        ptr::write(&raw mut NEXT_KTIMER, ptr::null_mut());
        ptr::write(&raw mut WAIT_KTIMER, WaitKTimer::inactive());
        (*KTIMER_QUEUE.get()).insert((*ptr::addr_of_mut!(WAIT_KTIMER)).entity_mut());
    });
}

pub unsafe fn enqueue_ktimer(entity: *mut KTimerEntity) {
    ktimer_critical(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
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

pub(crate) unsafe fn program_wait_ktimer() {
    ktimer_critical(|| unsafe {
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
            let wait_thread = pop_expired_wait_thread(queue.now_ticks());
            if wait_thread.is_null() {
                break;
            }

            (*wait_thread).state = ThreadState::Ready;
            if (*wait_thread).is_cfs {
                enqueue_runq_from_waitq(wait_thread);
            } else {
                let ktimer_entity = rt_ktimer_entity(wait_thread);
                let rt_thread = rt_thread_from_thread_ctx(wait_thread);

                (*rt_thread).runtime += elapsed;

                (*ktimer_entity).set_active(true);
                (*ktimer_entity).set_deadline_after(
                    queue.now_ticks(),
                    (*ktimer_entity)
                        .duration()
                        .saturating_sub((*rt_thread).runtime),
                );
                (*ktimer_entity).reset_links();
                queue.insert(ktimer_entity);
            }
        }
    }
}

unsafe fn remove_ktimer(entity: *mut KTimerEntity) -> *mut KTimerEntity {
    ktimer_critical(|| unsafe {
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
    ktimer_critical(|| unsafe {
        if entity.is_null() {
            return;
        }

        let queue = &mut *KTIMER_QUEUE.get();
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
            (*entity).miss_cnt = (*entity).miss_cnt.saturating_add(1);
            (*entity).set_deadline_after(queue.now_ticks(), (*entity).duration());
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

pub fn dequeue_ktimerq_to_waitq(thread: *mut ThreadCtx) -> Result<(), WaitQueueError> {
    ktimer_critical(|| unsafe {
        if thread.is_null() || (*thread).is_cfs {
            return Err(WaitQueueError::NotFound);
        }

        let ktimer_entity = rt_ktimer_entity(thread);
        if ktimer_entity.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        remove_ktimer(ktimer_entity);
        (*thread).state = ThreadState::Waiting;

        insert_wait_thread(thread);
        program_wait_ktimer();

        Ok(())
    })
}

pub fn enqueue_ktimerq_from_waitq(thread: *mut ThreadCtx) -> Result<(), WaitQueueError> {
    ktimer_critical(|| unsafe {
        if thread.is_null() || (*thread).is_cfs {
            return Err(WaitQueueError::NotFound);
        }

        let ktimer_entity = rt_ktimer_entity(thread);
        if ktimer_entity.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        remove_wait_thread(thread);

        (*thread).state = ThreadState::Ready;
        reinsert_ktimer(ktimer_entity);
        program_wait_ktimer();

        Ok(())
    })
}

pub fn next_ktimer_deadline() -> Option<u32> {
    ktimer_critical(|| unsafe { (*KTIMER_QUEUE.get()).next_deadline() })
}

pub fn next_ktimer_reload() -> Option<u32> {
    ktimer_critical(|| unsafe { (*KTIMER_QUEUE.get()).next_reload() })
}

pub(crate) fn elapsed_ticks_since_last_interrupt() -> u32 {
    SYST::get_reload().saturating_add(1)
}

pub(crate) fn elapsed_ticks_since_current_reload() -> u32 {
    SYST::get_reload().saturating_sub(SYST::get_current())
}

pub(crate) unsafe fn advance_ktimers(elapsed: u32) {
    ktimer_critical(|| unsafe {
        (*KTIMER_QUEUE.get()).advance_time(elapsed);
    });
}

pub(crate) unsafe fn dispatch_expired_ktimer(elapsed: u32) -> *mut KTimerEntity {
    ktimer_critical(|| unsafe { (*KTIMER_QUEUE.get()).dispatch_expired(elapsed) })
}

pub(crate) unsafe fn update_next_ktimer(entity: *mut KTimerEntity) {
    ktimer_critical(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        NEXT_KTIMER = normalize_next_ktimer(queue, entity);
    });
}

pub fn traverse_ktimer_queue() {
    ktimer_critical(|| unsafe {
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
    ktimer_critical(|| unsafe {
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
    ktimer_critical(|| unsafe {
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
    ktimer_critical(|| unsafe { NEXT_KTIMER })
}

pub(crate) fn ktimer_now_ticks() -> u64 {
    ktimer_critical(|| unsafe { (*KTIMER_QUEUE.get()).now_ticks() })
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
    ktimer_critical(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        if entity.is_null() {
            return ptr::null_mut();
        }

        queue.remove(entity);

        if is_cfs_ktimer(entity) {
            (*entity).set_deadline_after(
                queue.now_ticks().saturating_add(u64::from(elapsed)),
                (*entity).duration().saturating_sub(elapsed),
            );
        } else {
            let current_rt_thread_ctx = (*KTimerEntity::rt_ktimer(entity)).thread_ctx();
            let current_rt_thread = rt_thread_from_thread_ctx(current_rt_thread_ctx);

            // Update currnt RtThread's deadline and check for deadline miss
            (*current_rt_thread).runtime += elapsed;
            if (*current_rt_thread).runtime > (*entity).duration() {
                crate::rtsched_println!(
                    "Deadline miss in thread '{}': runtime {} ticks exceeded timer duration {} ticks",
                    (*current_rt_thread).thread.name,
                    (*current_rt_thread).runtime,
                    (*entity).duration()
                );
                (*entity).miss_cnt += 1;
            }

            (*entity).set_deadline_after(
                queue.now_ticks().saturating_add(u64::from(elapsed)),
                (*entity)
                    .duration()
                    .saturating_sub((*current_rt_thread).runtime),
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
            // TODO:
            // If there no more active timers, select timer
            // for cpu_idle thread.
            activate_cfs_ktimer()
        } else {
            next
        }
    })
}

pub(crate) fn program_next_systick() -> Option<u32> {
    ktimer_critical(|| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        let entity = queue.first();

        let reload = if is_cfs_ktimer(entity) {
            (*KTimerEntity::cfs_ktimer(entity)).execution_ticks()
        } else if is_wait_ktimer(entity) {
            (*entity).remaining_at(queue.now_ticks())
        } else {
            (*entity).remaining_at(queue.now_ticks())
        };

        (*SYST::PTR).rvr.write(reload + 1);
        (*SYST::PTR).cvr.write(0);

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
    now_ticks: u64,
}

impl KTimerQueue {
    pub const fn new() -> Self {
        Self {
            tree: RBTree::new(),
            now_ticks: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tree.len()
    }

    pub fn root(&self) -> *mut KTimerEntity {
        self.tree.root()
    }

    pub fn first(&self) -> *mut KTimerEntity {
        self.tree.first()
    }

    pub fn last(&self) -> *mut KTimerEntity {
        self.tree.last()
    }

    pub fn next(&self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        self.tree.next(entity)
    }

    pub fn now_ticks(&self) -> u64 {
        self.now_ticks
    }

    pub fn next_deadline(&self) -> Option<u32> {
        let first = self.first();
        if first.is_null() {
            None
        } else {
            Some(unsafe { (*first).remaining_at(self.now_ticks) })
        }
    }

    pub fn next_reload(&self) -> Option<u32> {
        self.next_deadline().and_then(reload_from_ticks)
    }

    pub fn advance_time(&mut self, elapsed: u32) {
        self.now_ticks = self.now_ticks.saturating_add(u64::from(elapsed));
    }

    pub unsafe fn dispatch_expired(&mut self, elapsed: u32) -> *mut KTimerEntity {
        unsafe {
            while let Some(expired) = self.pop_first() {
                let expired = expired as *mut KTimerEntity;
                if !(*expired).is_expired_at(self.now_ticks) {
                    self.insert(expired);
                    break;
                }

                if is_wait_ktimer(expired) {
                    wake_wait_thread(self, elapsed);
                    update_wait_ktimer_deadline(expired);
                    self.insert(expired);
                } else if is_cfs_ktimer(expired) {
                    if (*expired).is_active() {
                        (*expired).set_deadline_after(
                            self.now_ticks,
                            (*expired).duration().saturating_sub(elapsed),
                        );
                        (*expired).set_active(false);
                        self.insert(expired);
                    } else {
                        (*expired).set_deadline_after(self.now_ticks, (*expired).duration());
                        (*expired).set_active(true);
                        self.insert(expired);
                    }
                } else {
                    let thread_ctx = (*KTimerEntity::rt_ktimer(expired)).thread_ctx();
                    let rt_thread = rt_thread_from_thread_ctx(thread_ctx);
                    if (*expired).is_active() {
                        crate::rtsched_println!(
                            "Deadline miss in thread '{}': runtime {} ticks exceeded timer duration {} ticks",
                            (*rt_thread).thread.name,
                            (*rt_thread).runtime,
                            (*expired).duration()
                        );
                        (*expired).miss_cnt += 1;
                    }
                    (*rt_thread).runtime = 0;
                    (*expired).set_deadline_after(self.now_ticks, (*expired).duration());
                    (*expired).set_active(true);
                    self.insert(expired);
                }
            }

            let next = self.first_active();
            if next.is_null() {
                // TODO: If no more active timers, run cpu idle thread
                activate_cfs_ktimer()
            } else {
                next
            }
        }
    }

    /// Insert a detached ktimer entity into the queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure `entity` is valid for mutation and is not already
    /// linked into a queue.
    pub unsafe fn insert(&mut self, entity: *mut KTimerEntity) {
        unsafe { self.tree.insert(entity) }
    }

    /// Remove a ktimer entity from the queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure `entity` currently belongs to this queue.
    pub unsafe fn remove(&mut self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        unsafe { self.tree.remove(entity) }
    }

    /// Remove and return the earliest ktimer entity in the queue.
    pub unsafe fn pop_first(&mut self) -> Option<&mut KTimerEntity> {
        unsafe { self.tree.pop_first() }
    }

    pub fn first_active(&self) -> *mut KTimerEntity {
        let mut entity = self.first();
        while !entity.is_null() {
            if unsafe { (*entity).is_active() } {
                return entity;
            }
            entity = self.next(entity);
        }

        ptr::null_mut()
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
    use std::vec::Vec;

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

    #[test]
    fn reload_from_ticks_converts_to_systick_reload() {
        assert_eq!(reload_from_ticks(0), None);
        assert_eq!(reload_from_ticks(1), Some(0));
        assert_eq!(reload_from_ticks(42), Some(41));
        assert_eq!(
            reload_from_ticks(CM_SYSTICK_RELOAD_MAX + 1),
            Some(CM_SYSTICK_RELOAD_MAX)
        );
        assert_eq!(reload_from_ticks(CM_SYSTICK_RELOAD_MAX + 2), None);
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
            CM_SYSTICK_RELOAD_MAX
        );
        assert_eq!(collect_remaining(&queue), [0, 15, CM_SYSTICK_RELOAD_MAX]);
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

        assert!(ptr::eq(queue.first_active(), &mut active_late));
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

        assert!(ptr::eq(removed, &mut timers[1]));
        assert!(!timers[1].is_linked());
        assert_eq!(queue.len(), 2);
        assert_eq!(collect_deadlines_at(&queue), [8, 12]);
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
}
