# KTimer framework

[Documentation index](../README.md) · Source: [`src/ktimer.rs`](../src/ktimer.rs)

The `KTimer` framework is the foundation for both CFS and RT scheduling. It uses
separate active and inactive red-black trees with `KTimerEntity` defined as:

```rust
pub struct KTimerEntity {
    expire_at: u64,
    node: RbNode,
    active: bool,
    timing: RtTiming,
}
```

`KTimerEntity` stores intrusive queue state and the next absolute timer
expiration. `KTimerEntity` also owns the `RtTiming` policy values used by both
CFS and RT timers. For CFS, `period_ticks` is the CFS period and
`relative_deadline_ticks` is the CFS execution slice.
RT miss accounting is stored on `RtKTimer`, because CFS and wait timers do not
have RT deadlines.
The embedded `KTimerEntity` is keyed by the next absolute expiration or release
time. `SysTick` programming works differently for `CfsKTimer` and `RtKTimer`.
When `CfsKTimer` switches to active, it programs `SysTick` with its execution slice.
When `CfsKTimer` is switched out, its `expire_at` is set to the end of the current CFS period and
the timer is marked `inactive`.

`RtThread` timing is split into three meanings:

- `period_ticks`: time between releases of consecutive jobs.
- `relative_deadline_ticks`: time from release to the job's scheduling deadline.
- `budget_ticks`: runtime budget checked against `RtThread::runtime`.

`expire_at` is the next timer expiration value and is updated when a timer is re-armed/rescheduled:
dispatch expiry in `SysTick` interrupt handler, `yieldyi`, wait timer programming in `msleepyi`.

SysTick stores a reload register value rather than a direct interval. A reload
value of `R` wraps after `R + 1` ticks, so an interval of `N` scheduler ticks is
programmed as `N - 1`. The Cortex-M reload register is 24 bits wide and
`rtsched` treats the writable reload range as `1..=0x00ff_ffff`. The raw
conversion from ticks may produce reload value `0` for a one-tick interval, but
scheduler programming writes `1` instead. When the next timer expiration is already due
or farther away than the hardware can represent, scheduler programming uses the
nearest writable SysTick reload value. Long expiration times remain stored as absolute
`u64` tick values; each SysTick interrupt advances the timer queue by the
programmed chunk and the scheduler reprograms the next chunk until the absolute
deadline is reached.

The scheduler tracks the hardware reload epoch separately from time already
charged by a cooperative yield. Reading the current time before PendSV
reprograms the timer does not add those elapsed ticks again. Message-queue
timeouts use this clock, including partial timer intervals, and keep one
absolute deadline across retries.

When `RtThread` completes its job, it should call `yieldyi` to enter
`ThreadState::ReadyInactive` and reset its `runtime`. Its timer moves from
`active_tree` to `inactive_tree` until the next `period_ticks` release. Dispatch
then moves it back to `active_tree` and sets `ThreadState::ReadyActive` before
the scheduler can select it. A preempted RT thread stays in `active_tree` and
becomes `ReadyActive`; blocking detaches its timer and enters `Waiting` or
`Suspended` until a wake restores `ReadyActive` in `active_tree`.
The scheduler accounts the final elapsed interval and calculates that release
using the completed job's runtime before clearing the counter. Application code
does not need to reset runtime at job entry. Sleeping or blocking within the
same job preserves accumulated runtime; the scheduler also initializes runtime
at thread creation and resets it when dispatching a periodic release.

Each timer's embedded `RbNode` belongs to exactly one of the two trees. Active
timers are eligible for scheduler selection; inactive timers track future
releases and the shared wait timer's next wakeup. `enqueue_active()` and
`enqueue_inactive()` set a detached timer's eligibility and insert it into the
matching tree together; there is no standalone active-state setter. Remove a
timer before updating its expiration or enqueueing it into the other tree.
Temporary `remove()` preserves its previous active state for expiry processing;
`dequeue()` clears eligibility when the timer leaves the queue to block.

Both trees cache their earliest timer. `first_active()` reads the active minimum
in O(1), and `first()` compares the two minima in O(1) to choose the next event
for SysTick programming or expiry dispatch. Removing the active minimum finds
its successor only in the active tree, without scanning inactive timers. Both
trees use the same `now_ticks` clock and preserve exact absolute expiration
ordering, including the entity-address tie-breaker. Combined diagnostic
traversal uses an O(log K) successor lookup per timer, where K is the timer count.

See [the scheduler guide](sched.md#soft-realtime-scheduler-for-rtthread) for
RT job release and completion, and [the CFS run queue guide](runq.md) for fair scheduling.
