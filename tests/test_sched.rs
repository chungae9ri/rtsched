use rtsched::test_support::sched;

#[test]
fn scheduler_started_latch_is_marked_explicitly() {
    sched::scheduler_started_latch_is_marked_explicitly();
}

#[test]
fn handle_sched_tick_ignores_queued_work_before_scheduler_start() {
    sched::handle_sched_tick_ignores_queued_work_before_scheduler_start();
}

#[test]
fn init_cfs_resets_run_queue_and_configures_cfs_timer() {
    sched::init_cfs_resets_run_queue_and_configures_cfs_timer();
}

#[test]
fn cfs_accounting_updates_vruntime_and_sched_ticks() {
    sched::cfs_accounting_updates_vruntime_and_sched_ticks();
}

#[test]
fn cfs_preempts_current_when_queued_thread_has_lower_vruntime() {
    sched::cfs_preempts_current_when_queued_thread_has_lower_vruntime();
}

#[test]
fn cfs_keeps_current_when_it_still_has_lower_vruntime() {
    sched::cfs_keeps_current_when_it_still_has_lower_vruntime();
}

#[test]
fn cfs_timer_switches_from_rt_thread_to_leftmost_cfs_thread() {
    sched::cfs_timer_switches_from_rt_thread_to_leftmost_cfs_thread();
}

#[test]
fn rt_timer_switches_from_cfs_thread_to_rt_thread_and_requeues_cfs() {
    sched::rt_timer_switches_from_cfs_thread_to_rt_thread_and_requeues_cfs();
}

#[test]
fn register_idle_thread_rejects_cfs_and_rt_threads() {
    sched::register_idle_thread_rejects_cfs_and_rt_threads();
}

#[test]
fn cfs_timer_switches_from_rt_thread_to_idle_when_run_queue_is_empty() {
    sched::cfs_timer_switches_from_rt_thread_to_idle_when_run_queue_is_empty();
}

#[test]
fn null_next_timer_switches_from_rt_thread_to_idle() {
    sched::null_next_timer_switches_from_rt_thread_to_idle();
}

#[test]
fn cfs_timer_switches_from_current_cfs_to_idle_when_run_queue_is_empty() {
    sched::cfs_timer_switches_from_current_cfs_to_idle_when_run_queue_is_empty();
}

#[test]
fn inactive_cfs_timer_falls_back_to_idle_without_popping_queued_cfs_thread() {
    sched::inactive_cfs_timer_falls_back_to_idle_without_popping_queued_cfs_thread();
}

#[test]
fn idle_thread_yields_to_queued_cfs_thread_without_entering_run_queue() {
    sched::idle_thread_yields_to_queued_cfs_thread_without_entering_run_queue();
}

#[test]
fn rt_timer_switches_from_idle_thread_without_requeueing_idle() {
    sched::rt_timer_switches_from_idle_thread_without_requeueing_idle();
}

#[test]
fn idle_fallback_requeues_current_cfs_thread() {
    sched::idle_fallback_requeues_current_cfs_thread();
}

#[test]
fn traverse_idle_thread_visits_registered_idle_thread() {
    sched::traverse_idle_thread_visits_registered_idle_thread();
}
