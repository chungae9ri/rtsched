# Rust runtime scheduler (rtsched)

`rtsched` is a runtime scheduler crate for thread management. It provides
the core pieces needed to create and switch between application threads on a
single microcontroller core. The architecture-specific layer currently targets
Cortex-M through `cortex-m` crate. That code is isolated under `src/arch/cm`,
so additional architecture backends can be added under `src/arch`. The embedded
examples additionally use `cortex-m-rt` for startup, exception entry points,
and linker/runtime support.

`rtsched` is meant to stay a small kernel crate rather than grow into an
integrated operating system. The public API provides thread creation,
scheduling, sleeping, waiting, and diagnostics hooks. The scheduler core owns the
intrusive run, timer, and wait queues. The platform layer supplies only the
CPU-specific pieces needed for critical sections, stack setup, timer
programming, and context switching.

The crate includes:

- `Earliest Deadline First (EDF)` scheduling through the `KTimer` framework.
- A kernel timer queue built on an intrusive red-black tree.
- CFS (Completely Fair Scheduler) style scheduled threads `CfsThread` through the `RunQueue` red-black tree.
- `RtThread` associated with a dedicated `KTimer` entry for `EDF` scheduling.
- `ThreadHandle` and `ThreadId` for referring to spawned threads without
  passing raw thread-context pointers through application code.
- `WaitQueue` red-black tree for threads in `Waiting` state.
- Thread spawn with a dedicated stack (`forkyi`).
- Registered idle thread fallback when no normal CFS or RT work is runnable.
- CPU resource yielding (`yieldyi`) to the next `active` scheduler timer/entity.
- Lightweight scheduler tracing counters and callbacks.
- Preemptive context switching support.
- `SysTick` integration for advancing timers and requesting scheduler dispatch.

`rtsched` is intended to be used by a board crate that owns hardware setup,
clock configuration, `SysTick` configuration, thread stack allocation, and
concrete thread storage. The board initializes the ktimer queue and CFS scheduler,
creates threads with dedicated stacks, keeps the returned `ThreadHandle` values,
registers the idle thread with `register_idle_thread`, then starts the first
thread with `spawn_main_thread`.
Network stacks, filesystems, USB, shells, logging frameworks, and application services
should live outside `rtsched` and use the kernel APIs rather than become part
of the crate. Example board integrations live in
[`rtsched-boards`](https://github.com/chungae9ri/rtsched-boards).
`
## High level features

| Feature area | What `rtsched` provides |
| --- | --- |
| Scheduling scheme | Mixed soft real-time and fair scheduling with EDF-style `RtThread` timers and CFS-style `CfsThread` run queues. |
| Timer framework | Intrusive red-black tree based `KTimer` queue for deadlines, CFS execution windows, RT releases, and sleep wakeups. |
| Thread management | Thread spawning with dedicated stacks, explicit `Ready`/`Running`/`Waiting` states, yielding, waiting, and idle-thread fallback. |
| Architecture portability | Scheduler core is separated from platform code through common traits; Cortex-M is implemented today and host stubs keep tests/docs usable. |
| Power management | Deadline-driven/tickless-style scheduling programs the next timer deadline, and board code can register a `cpu_idle` thread to enter low-power states such as `wfi` when no normal CFS or RT work is runnable. |
| Diagnostics | Lightweight tracing counters, optional callbacks, cycle-counter helpers, and scheduler timing diagnostics. |

## Platform common traits

The architecture layer is exposed through small common traits so scheduler code
does not call `cortex-m` APIs directly. These traits describe the platform
capabilities required by the scheduler core:

- `ThreadStackPort`: builds the initial stack frame for a new scheduler thread.
- `CriticalSectionPort`: protects scheduler globals from interrupt or test-thread
  interleaving.
- `ContextSwitchPort`: requests a dispatch and starts the first scheduler
  thread.
- `SchedulerTimerPort`: reads and programs the reloadable scheduler timer used
  by the `KTimer` queue.
- `CycleCounterPort`: provides cycle-counter based elapsed-time diagnostics.

`Platform` is the combined contract for a Cortex-M-style scheduler port, and
`DefaultPlatform` selects the implementation for the current build target.
On ARM targets it uses `CortexMPlatform`, which is backed by SysTick, PendSV/SVC,
interrupt masking, and the DWT cycle counter. On non-ARM targets it uses
`HostPlatform`, which keeps tests and documentation builds usable while hardware
operations such as `spawn_main_thread` remain unavailable.

The common trait types are re-exported from the crate root together with
`InitialThreadContext`, `CortexMPlatform`, `HostPlatform`, `DefaultPlatform`,
and the public platform helpers such as `spawn_main_thread`,
`init_dwt_cycle_counter`, `dwt_cycle_count`, `get_elapse_cycles`, and
`get_elapse_msec`.

## Host Tests

The scheduler data-structure tests run on the native host target. From the
workspace root, use:

```sh
cargo test --manifest-path rtsched/Cargo.toml --target x86_64-unknown-linux-gnu
```

The explicit target matters for board-oriented workspaces because Cargo may
otherwise inherit an embedded default target, which cannot build Rust's host
test harness. On a non-x86 Linux host, replace `x86_64-unknown-linux-gnu` with
the `host:` value from `rustc -vV`.

## Error handling policy

`rtsched` uses three failure styles:

- Return `Result` or `Option` for runtime state that board code can handle, such as
  wait-queue transitions, missing current RT runtime, and missing next timer reload.
- Panic for public setup mistakes that would make scheduler state invalid, such as
  zero CFS priority, a null RT timer, null thread storage, null stack storage,
  too-small stacks, or unaligned stack tops. The thread builders also provide
  `try_spawn` variants that return `ThreadSpawnError` for these setup checks.
- Use `debug_assert!` for internal unsafe invariants that should already be guaranteed by
  safe wrappers or scheduler ownership rules, such as intrusive-tree duplicate insertion
  checks and raw pointer downcasts.

## Thread States

Each thread has an explicit `ThreadState`:

- `Ready`: eligible to run when selected by the scheduler.
- `Running`: currently executing on the CPU.
- `Waiting`: blocked until a timeout or wait condition is satisfied.

Scheduler code uses `ThreadCtx::set_state(ThreadState)` for runtime state changes.
Normal transitions are `Ready -> Running`, `Running -> Ready`,
`Running -> Waiting`, `Ready -> Waiting`, and `Waiting -> Ready`. A waiting
thread must return to `Ready` before it can run again; `Waiting -> Running` is
not a valid direct transition.

## Scheduler Tracing

Tracing hooks are available for scheduler event counters and optional callbacks:

- `trace_counters()` returns saturating counters for context switches, RT
  deadline misses, wakeups, and cooperative yields.
- `reset_trace_counters()` resets those counters to zero.
- `set_trace_fn(fn(TraceEvent))` registers a lightweight callback for each
  traced event, and `clear_trace_fn()` removes it.

Trace callbacks run from scheduler paths that may be inside a critical section
or interrupt-triggered context switch path, so callbacks should stay short,
non-blocking, and allocation-free.

## KTimer framework

The `KTimer` framework is the foundation for both CFS and RT scheduling. It builds a
red-black tree with `KTimerEntity` defined as:
```
pub struct KTimerEntity {
    deadline_at: u64,
    node: RbNode,
    active: bool,
    pub miss_cnt: u32,
}
```
`KTimerEntity` stores intrusive queue state and the next absolute timer
expiration. Timing policy lives in the owning timer type: `CfsKTimer` keeps the
CFS `period_ticks`/`execution_ticks`, and `RtKTimer` keeps explicit
`period_ticks`, `relative_deadline_ticks`, and `budget_ticks` values.
The embedded `KTimerEntity` is keyed by the next absolute deadline or release
time. `SysTick` programming works differently for `CfsKTimer` and `RtKTimer`.
When `CfsKTimer` switches to active, it programs `SysTick` with its `execution_ticks`.
When `CfsKTimer` is switched out, its `deadline_at` is set to the end of the current CFS period and
the timer is marked `inactive`.

`RtThread` timing is split into three meanings:

- `period_ticks`: time between releases of consecutive jobs.
- `relative_deadline_ticks`: time from release to the job's scheduling deadline.
- `budget_ticks`: runtime budget checked against `RtThread::runtime`.

`deadline_at` is the next timer expiration value and is updated when a timer is re-armed/rescheduled:
dispatch expiry in `SysTick` interrupt handler, `yieldyi`, wait timer programming in `msleepyi`.

SysTick stores a reload register value rather than a direct interval. A reload
value of `R` wraps after `R + 1` ticks, so an interval of `N` scheduler ticks is
programmed as `N - 1`. The Cortex-M reload register is 24 bits wide and
`rtsched` treats the writable reload range as `1..=0x00ff_ffff`. The raw
conversion from ticks may produce reload value `0` for a one-tick interval, but
scheduler programming writes `1` instead. When the next deadline is already due
or farther away than the hardware can represent, scheduler programming uses the
nearest writable SysTick reload value. Long deadlines remain stored as absolute
`u64` tick values; each SysTick interrupt advances the timer queue by the
programmed chunk and the scheduler reprograms the next chunk until the absolute
deadline is reached.

When `RtThread` completes its job, it should call `yieldyi` to make itself inactive and to reset its
`runtime`. The inactive RT timer is parked until the next `period_ticks` release.

`RbNode` is the entry to the `KTimer` rbtree.

`active` timers are eligible for scheduler selection; inactive timers remain in the tree but are skipped by
`first_active()`.

## CFS (Completely Fair Scheduler) Scheduler

CFS scheduler assigns the CFS time slot to all CFS tasks based on the priority-based
virtual runtime (`vruntime`). CFS priority is inverse-numeric: `1` is the most
favored priority, and larger numeric values are less favored.

`vruntime` of each CFS thread is charged as:
vruntime = (ticks_consumed * priority) / priority_sum_of_all_CFS_threads

Because the scheduler selects the CFS thread with the smallest `vruntime`, lower
numeric `priority` values are favored because their `vruntime` grows more slowly.
For example, with the same `priority_sum`, a thread with priority `1` accumulates
one fourth as much `vruntime` as a thread with priority `4` for the same elapsed
ticks.

CFS scheduler doesn't starve less-favored threads because even a thread with a
larger numeric priority gets a minimum CPU resource slot for running.

CFS threads are moved between the `RunQueue` and the `WaitQueue` rbtree by using
`RbNode` in the `SchedEntity`.

CFS has a dedicated `CfsKTimer` with `period_ticks` and `execution_ticks`. `execution_ticks` is the
time slice for one CFS scheduling window.

CFS scheduling is used for non-time critical threads such as shell thread for user interaction.

The idle thread is a CFS thread registered through `register_idle_thread`. It is
removed from the CFS run queue and does not participate in CFS fairness
accounting. The scheduler selects it only when no normal CFS thread is runnable
and no active RT timer should run. Diagnostics can inspect it through
`traverse_idle_thread_fn`.

## Soft Realtime Scheduler for RtThread

Each `RtThread` has its own `RtKTimer` entry in the `KTimer` red-black tree (rbtree).

`RtThread` should complete its job before the deadline and yield (`yieldyi`) CPU ownership to the next thread
at the left-most entry in the `KTimer` rbtree. Active RT timers are ordered by
their absolute job deadline, computed from `relative_deadline_ticks`. When the
current `RtThread` yields after finishing a job, it is set to `inactive` and
reinserted with a new `deadline_at` based on the next `period_ticks` release. It
becomes active again when that release timer expires, with a fresh relative
deadline.

`RtKTimer::new(period_ticks, ...)` keeps the compatibility behavior where period,
relative deadline, and budget all use the same tick value. Use
`RtKTimer::new_with_timing(RtTiming::new(period_ticks, relative_deadline_ticks,
budget_ticks), ...)` when those meanings differ.

## `cpu_idle` Thread for Power Saving

Board code can register a CFS thread as the idle thread with `register_idle_thread()`.
The idle thread is removed from the normal CFS run queue and is selected only as a scheduler fallback.

The scheduler selects `cpu_idle` when no RT timer is selected to run and either:

- the CFS ktimer is inactive, meaning the current CFS execution window is closed
- the CFS run queue is empty, meaning all normal CFS threads are waiting or no
  normal CFS thread has been spawned

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

## Embedded example programs

The `rtsched/examples` directory contains small `no_std` Cortex-M programs that
show how board code wires the scheduler together. They are intentionally
board-neutral: each example initializes the scheduler, creates statically
allocated threads and stacks, registers `cpu_idle`, programs SysTick from
`next_ktimer_reload()`, and handles SysTick with `handle_sched_tick()`. The
examples use atomic counters and spin loops as simple observable work instead
of board-specific UART or LED drivers.

All examples share `examples/common/mod.rs`, which provides:

- clock constants for a 12 MHz system clock
- common CFS period and execution-window ticks
- `init_scheduler()` for `update_sys_clk_freq()`, `init_ktimer_queue()`, and
  `init_cfs()`
- `configure_systick()` for Cortex-M SysTick setup
- a `cpu_idle` thread that waits with `wfi`
- a panic handler that parks the CPU in idle

`minimal_cfs.rs` demonstrates the smallest normal CFS setup. It creates one
registered `cpu_idle` CFS thread and one runnable `worker` CFS thread. The
worker increments `WORKER_RUNS`, spins briefly, and calls `yieldyi()` so the
scheduler can select the next runnable entity.

`minimal_rt.rs` demonstrates one periodic RT thread. The `control` thread uses
`RtKTimer::new_with_timing()` with a 50 ms period, 20 ms relative deadline, and
5 ms budget. Each job resets the RT runtime counter with
`set_rt_thread_start_time(0)`, increments `CONTROL_JOBS`, performs a short spin,
and calls `yieldyi()` to finish the current job window.

`mixed_rt_cfs.rs` demonstrates RT and CFS work sharing the same scheduler. It
creates a background CFS thread plus two RT threads: `fast_rt` with a 40 ms
period, 15 ms deadline, and 4 ms budget, and `slow_rt` with a 100 ms period,
60 ms deadline, and 10 ms budget. The background thread sleeps for 100 ms with
`msleepyi()`, while the RT threads run jobs and yield at completion.

`sleep_wake.rs` demonstrates the wait queue path for CFS threads. It creates two
CFS sleeper threads. `fast_sleeper` increments `FAST_WAKEUPS` and sleeps for
50 ms; `slow_sleeper` increments `SLOW_WAKEUPS` and sleeps for 250 ms. SysTick
advances the timer queue and wakes each thread through the wait queue when its
sleep deadline expires.

Compile-check every embedded example with:

```sh
cargo check -p rtsched --examples --features embedded-examples --target thumbv8m.main-none-eabihf
```

To build one example for an LPC55S69 board, pass the board crate directory as a
linker search path so `cortex-m-rt` can find `memory.x`:

```sh
cargo rustc -p rtsched \
  --example minimal_cfs \
  --features embedded-examples \
  --target thumbv8m.main-none-eabihf \
  -- -L boards/lpc55s69
```

Flash the produced ELF with `probe-rs`:

```sh
probe-rs download \
  --chip LPC55S69JBD100 \
  --protocol swd \
  target/thumbv8m.main-none-eabihf/debug/examples/minimal_cfs
```

Replace `minimal_cfs` with `minimal_rt`, `mixed_rt_cfs`, or `sleep_wake` to run
the other examples.

