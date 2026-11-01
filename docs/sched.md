# Scheduling

[Documentation index](../README.md) · Source: [`src/sched.rs`](../src/sched.rs)

The scheduler combines [CFS background work](runq.md) with EDF-style soft
real-time jobs using the [KTimer framework](ktimer.md). Board code initializes
the timer queue with `init_ktimer_queue()`, configures CFS with `init_cfs()`,
and calls `handle_sched_tick()` from SysTick to advance timers and request dispatch.

## Soft Realtime Scheduler for RtThread

Each `RtThread` has its own `RtKTimer` entry in one of the `KTimer` red-black trees.

`RtThread` should complete its job before the deadline and yield (`yieldyi`) CPU ownership to the next thread
at the left-most entry in the active `KTimer` rbtree. Active RT timers are ordered by
their absolute job deadline, computed from `relative_deadline_ticks`. When the
current `RtThread` yields after finishing a job, it is set to `inactive` and
reinserted into the inactive tree with a new `expire_at` based on the next
`period_ticks` release. When that release timer expires, it moves back to the
active tree with a fresh relative deadline.

`RtKTimer::new(period_ticks, ...)` keeps the compatibility behavior where period,
relative deadline, and budget all use the same tick value. Use
`RtKTimer::new_with_timing(RtTiming::new(period_ticks, relative_deadline_ticks,
budget_ticks), ...)` when those meanings differ.

## `cpu_idle` Thread for Power Saving

The idle thread is a dedicated `IdleThread` initialized and registered by
`spawn_main_thread`. It does not participate in CFS fairness accounting or RT
timer scheduling. The scheduler selects it only when no normal scheduler work is
runnable. Diagnostics can inspect it through `traverse_idle_thread_fn`.

Board code starts a registered idle thread with `spawn_main_thread()`. The helper
initializes the idle thread storage and stack, registers it as the scheduler
fallback, and restores it as the first running thread. The idle thread is
selected only as a scheduler fallback.

The scheduler selects `cpu_idle` when:

- no active scheduler timer is selected, so `NEXT_KTIMER` is null
- the CFS ktimer is selected but the CFS run queue is empty, meaning all normal
  CFS threads are waiting, or no normal CFS thread has been spawned

Application code can put low-power behavior such as `wfi` in the idle thread.

## Example of scheduling

C: runtime needed to finish one job
D: relative deadline of the thread
T: period between job releases

The examples below use `T = D`.

example 1:

```text
Ta: C=2, D=5
Tb: C=3, D=10
0     1     2     3     4     5     6     7     8     9     10
|-----Ta----|--------Tb-------|-----Ta----|-------idle------|
```

example 2:

```text
Ta: C=2, D=5
Tb: C=3, D=9
Tc: C=1, D=6
0     1     2     3     4     5     6     7     8     9
|-----Ta----|-Tc--|-----Tb----|-Ta--|-Tc--|-Tb--|-Ta--|
```
