# CFS (Completely Fair Scheduler) run queue

[Documentation index](../README.md) · Source: [`src/runq.rs`](../src/runq.rs)

CFS scheduler assigns the CFS time slot to all CFS tasks based on the priority-based
virtual runtime (`vruntime`). CFS priority is inverse-numeric: `1` is the most
favored priority, and larger numeric values are less favored.

`vruntime` of each CFS thread is charged as:

```text
vruntime = (ticks_consumed * priority) / priority_sum_of_all_CFS_threads
```

Because the scheduler selects the CFS thread with the smallest `vruntime`, lower
numeric `priority` values are favored because their `vruntime` grows more slowly.
For example, with the same `priority_sum`, a thread with priority `1` accumulates
one fourth as much `vruntime` as a thread with priority `4` for the same elapsed
ticks.

CFS scheduler doesn't starve less-favored threads because even a thread with a
larger numeric priority gets a minimum CPU resource slot for running.

CFS threads move between the `RunQueue` and the [wait queue](waitq.md).
The run queue uses the intrusive `RbNode` in each thread's `SchedEntity`;
blocked threads use their separate `WaitEntity` links.

CFS has a dedicated `CfsKTimer` whose embedded `KTimerEntity` stores
`period_ticks` and the execution slice in `relative_deadline_ticks`. The
execution slice is the time slice for one CFS scheduling window.

CFS scheduling is used for non-time critical threads such as shell thread for user interaction.

See [kernel timers](ktimer.md) for CFS timer programming and
[the scheduler guide](sched.md#cpu_idle-thread-for-power-saving) for idle-thread fallback.
