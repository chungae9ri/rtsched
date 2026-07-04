# Rust runtime scheduler (rtsched)

`rtsched` is a runtime scheduler crate for thread management. It provides
the core pieces needed to create and switch between application threads on a
single microcontroller core.

The crate includes:

- `Earliest Deadline First (EDF)` scheduling through the `KTimer` framework.
- A kernel timer queue built on an intrusive red-black tree.
- CFS (Completely Fair Scheduler) style scheduled threads `CfsThread` through the `RunQueue` red-black tree.
- `RtThread` associated with a dedicated `KTimer` entry for `EDF` scheduling.
- `WaitQueue` red-black tree for threads in `Waiting` state.
- Thread spawn with a dedicated stack (`forkyi`).
- CPU resource yielding (`yieldyi`) to the next `active` scheduler timer/entity.
- Preemptive context switching support.
- `SysTick` integration for advancing timers and requesting scheduler dispatch.

`rtsched` is intended to be used by a board crate that owns hardware setup,
clock configuration, `SysTick` configuration, thread stack allocation, and
concrete thread storage. The board initializes the ktimer queue and CFS scheduler,
creates threads with dedicated stacks, then starts the first thread with `spawn_main_thread`.

## KTimer framework

The `KTimer` framework is the foundation for both CFS and RT scheduling. It builds a
red-black tree with `KTimerEntity` defined as:
```
pub struct KTimerEntity {
    duration: KTimerDuration,
    deadline_at: u64,
    node: RbNode,
    active: bool,
    pub miss_cnt: u32,
}
```
The `duration` field is the scheduling period for timers (`RtKTimer` and `CfsKTimer`). `SysTick`
programming works differently for `CfsKTimer` and `RtKTimer`.
When `CfsKTimer` switches to active, it programs `SysTick` with its `execution_ticks`.
When `CfsKTimer` is switched out, its `deadline_at` is set to the end of the current CFS period and
the timer is marked `inactive`.

`RtThread` has its own duration value for its periodic scheduling and should finish its task within this duration.

`deadline_at` is the next timer expiration value and is updated when a timer is re-armed/rescheduled:
dispatch expiry in `SysTick` interrupt handler, `yieldyi`, wait timer programming in `msleepyi`.

When `RtThread` completes its job, it should call `yieldyi` to make itself inactive and to reset its
`runtime`, `deadline_at`.

`RbNode` is the entry to the `KTimer` rbtree.

`active` timers are eligible for scheduler selection; inactive timers remain in the tree but are skipped by
`first_active()`.

## CFS (Completely Fair Scheduler) Scheduler

CFS scheduler assigns the CFS time slot to all CFS tasks based on the priority-based
virtual runtime (`vruntime`).
`vruntime` of each CFS thread is defined as:
vruntime = (ticks_consumed * priority) / priority_sum_of_all_CFS_threads

Lower numeric `priority` values are favored because their `vruntime` grows more slowly. CFS scheduler
makes this vruntime fair among the CFS threads.

CFS scheduler doesn't starve lower-priority threads because even the lowest-priority thread gets a minimum
CPU resource slot for running.

CFS threads are moved between the `RunQueue` and the `WaitQueue` rbtree by using
`RbNode` in the `SchedEntity`.

CFS has a dedicated `CfsKTimer` with `execution_ticks` and `duration`. `execution_ticks` is the
time slice for one CFS scheduling window.

CFS scheduling is used for non-time critical threads such as shell thread for user interaction.

## Soft Realtime Scheduler for RtThread

Each `RtThread` has its own entry with duration and `deadline_at` in the `KTimer` red-black tree (rbtree).

`RtThread` should complete its job before the deadline and yield (`yieldyi`) CPU ownership to the next thread
at the left-most entry in the `KTimer` rbtree. When the current `RtThread` yields, current `RtThread` is
set to `inactive` and reinserted with a new `deadline_at` based on the post-yield `now_ticks` plus the
unused ticks in its duration. It becomes active again when that timer expires.

## Example of scheduling

C: runtime needed to finish one job
D: periodic time (duration) of the thread (initial deadline)

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
