use rtsched::test_support::sync;

#[test]
fn sync_type_wait_events_decode_named_tags() {
    sync::sync_type_wait_events_decode_named_tags();
}

#[test]
fn sync_waiters_pop_by_earliest_deadline_not_insertion_order() {
    sync::sync_waiters_pop_by_earliest_deadline_not_insertion_order();
}

#[test]
fn blocking_sync_take_copies_current_scheduler_deadline() {
    sync::blocking_sync_take_copies_current_scheduler_deadline();
}

#[test]
fn mutex_lock_boosts_rt_owner_deadline_and_unlock_restores_it() {
    sync::mutex_lock_boosts_rt_owner_deadline_and_unlock_restores_it();
}

#[test]
fn mutex_lock_boosts_rt_owner_to_earliest_scheduler_deadline_and_unlock_restores_it() {
    sync::mutex_lock_boosts_rt_owner_to_earliest_scheduler_deadline_and_unlock_restores_it();
}

#[test]
fn mutex_lock_boosts_cfs_owner_to_earliest_scheduler_deadline_and_unlock_restores_it() {
    sync::mutex_lock_boosts_cfs_owner_to_earliest_scheduler_deadline_and_unlock_restores_it();
}

#[test]
fn binary_semaphore_try_take_and_give_track_single_token() {
    sync::binary_semaphore_try_take_and_give_track_single_token();
}

#[test]
fn binary_semaphore_take_blocks_current_cfs_thread_until_give() {
    sync::binary_semaphore_take_blocks_current_cfs_thread_until_give();
}

#[test]
fn binary_semaphore_take_without_current_thread_reports_error() {
    sync::binary_semaphore_take_without_current_thread_reports_error();
}

#[test]
fn counting_semaphore_try_take_and_give_track_bounded_tokens() {
    sync::counting_semaphore_try_take_and_give_track_bounded_tokens();
}

#[test]
fn counting_semaphore_take_blocks_current_cfs_thread_until_give() {
    sync::counting_semaphore_take_blocks_current_cfs_thread_until_give();
}

#[test]
fn counting_semaphore_take_without_current_thread_reports_error() {
    sync::counting_semaphore_take_without_current_thread_reports_error();
}

#[test]
#[should_panic(expected = "counting semaphore max_count must be non-zero")]
fn counting_semaphore_rejects_zero_max_count() {
    sync::counting_semaphore_rejects_zero_max_count();
}

#[test]
#[should_panic(expected = "counting semaphore initial_count must not exceed max_count")]
fn counting_semaphore_rejects_initial_count_above_max_count() {
    sync::counting_semaphore_rejects_initial_count_above_max_count();
}

#[test]
fn mutex_try_lock_protects_data_and_unlocks_on_drop() {
    sync::mutex_try_lock_protects_data_and_unlocks_on_drop();
}

#[test]
fn mutex_try_lock_reports_would_block_for_other_owner() {
    sync::mutex_try_lock_reports_would_block_for_other_owner();
}

#[test]
fn mutex_lock_blocks_waiter_and_drop_transfers_ownership() {
    sync::mutex_lock_blocks_waiter_and_drop_transfers_ownership();
}

#[test]
fn mutex_lock_without_current_thread_reports_error() {
    sync::mutex_lock_without_current_thread_reports_error();
}

#[test]
fn mutex_unique_access_helpers_skip_scheduler_locking() {
    sync::mutex_unique_access_helpers_skip_scheduler_locking();
}
