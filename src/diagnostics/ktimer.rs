// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Diagnostic helpers for ktimer queue inspection and deadline misses.

use core::ptr;

use crate::ktimer::{
    KTimerEntity, KTimerQueue, is_cfs_ktimer, is_wait_ktimer, ktimer_name, with_ktimer_queue,
};
use crate::sync::SyncType;
use crate::thread::{
    CfsThread, RtThread, ThreadCtx, ThreadHandle, ThreadRef, ThreadState, cfs_thread_from_handle,
    rt_ktimer_entity, rt_thread_from_handle,
};

pub fn traverse_ktimer_queue() {
    with_ktimer_queue(|queue| unsafe {
        let mut entity = queue.first();

        crate::rtsched_println!("ktimer queue:");
        while !entity.is_null() {
            crate::rtsched_println!(
                "{} ktimer's remaining={}, active={}",
                ktimer_name(entity),
                (*entity).remaining_at(queue.now_ticks()),
                (*entity).is_active()
            );
            entity = queue.next(entity);
        }
    });
}

/// Traverse the ktimer queue and invoke `f` for each ktimer with its name
/// and remaining ticks. This is similar to `traverse_ktimer_queue` but allows the
/// caller to handle formatting/output (for example, writing to UART).
pub fn traverse_ktimer_queue_fn<F>(mut f: F)
where
    F: FnMut(&'static str, u32),
{
    with_ktimer_queue(|queue| unsafe {
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

pub(crate) unsafe fn print_rt_deadline_miss_diagnostics(
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
        if !is_cfs_ktimer(entity) && !is_wait_ktimer(entity) {
            let rt_ktimer = KTimerEntity::container_of(entity);
            crate::rtsched_println!(
                "  {} name='{}' kind={} queued={} expire_at={} remaining={} active={} misses={}",
                marker,
                ktimer_name(entity),
                ktimer_kind_name(entity),
                yes_no(queued),
                (*entity).expire_at(),
                (*entity).remaining_at(queue.now_ticks()),
                yes_no((*entity).is_active()),
                (*rt_ktimer).miss_cnt
            );

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
        } else {
            crate::rtsched_println!(
                "  {} name='{}' kind={} queued={} expire_at={} remaining={} active={}",
                marker,
                ktimer_name(entity),
                ktimer_kind_name(entity),
                yes_no(queued),
                (*entity).expire_at(),
                (*entity).remaining_at(queue.now_ticks()),
                yes_no((*entity).is_active())
            );
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

        if crate::sched::is_idle_thread(current) {
            print_basic_thread_statistics("  ", "current", &*current, "idle");
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

        print_basic_thread_statistics("  ", "idle", &*idle, "idle");
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

fn print_basic_thread_statistics(
    indent: &str,
    label: &str,
    thread_ctx: &ThreadCtx,
    class: &'static str,
) {
    crate::rtsched_println!(
        "{}{}: id={} name='{}' class={} state={}",
        indent,
        label,
        thread_ctx.id,
        thread_ctx.name,
        class,
        thread_state_name(thread_ctx.state)
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
        let rt_ktimer = KTimerEntity::container_of(ktimer_entity);
        crate::rtsched_println!(
            "{}{}: id={} name='{}' class=rt state={} runtime={} ktimer='{}' expire_at={} remaining={} active={} misses={}",
            indent,
            label,
            thread_ctx.id,
            thread_ctx.name,
            thread_state_name(thread_ctx.state),
            thread.runtime(),
            ktimer_name(ktimer_entity),
            (*ktimer_entity).expire_at(),
            (*ktimer_entity).remaining_at(now_ticks),
            yes_no((*ktimer_entity).is_active()),
            (*rt_ktimer).miss_cnt
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
