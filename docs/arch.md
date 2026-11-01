# Platform common traits

[Documentation index](../README.md) · Source: [`src/arch`](../src/arch)

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
