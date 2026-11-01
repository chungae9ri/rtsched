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

`Platform` combines these capabilities into the scheduler's platform contract.
`DefaultPlatform` uses the backend selected by the standard architecture and
Cargo feature guards.
The `host` Cargo feature explicitly selects `HostPlatform` for native tests,
benchmarks, and documentation builds on any host CPU and OS. Hardware operations
such as `spawn_main_thread` remain unavailable in this test backend.

Without `host`, ARM targets select `CortexMPlatform`, which is backed by SysTick,
PendSV/SVC, interrupt masking, and the DWT cycle counter. Other targets require
their own hardware port; they do not fall back to the host backend.

## Source layout

- [`build.rs`](../build.rs) validates that the target has a hardware port or
  the `host` feature is enabled.
- [`src/arch/platform.rs`](../src/arch/platform.rs) defines the shared traits,
  aliases `DefaultPlatform` to the selected backend, and dispatches scheduler calls.
- [`src/arch/cm`](../src/arch/cm) contains `CortexMPlatform`, its hardware
  implementations, stack initialization, and the SVCall/PendSV context-switch
  assembly.
- [`src/arch/host`](../src/arch/host) contains `HostPlatform`, the reentrant host
  critical section, synthetic test stack frames, and timer/cycle-counter stubs.

The `cm` module and its `CortexMPlatform` re-export use
`all(target_arch = "arm", not(feature = "host"))`; the `host` module and its
`HostPlatform` re-export use `feature = "host"`. Host selection is explicit and
independent of other platforms. The feature takes precedence, so ARM host
machines can run native tests without including the Cortex-M implementation.

`target_arch` identifies the CPU, such as `arm`, `x86_64`, or `riscv32`; it
cannot identify a host test backend that runs on any CPU. Adding a RISC-V or
Cadence HiFi port requires its own architecture guard and target validation in
`build.rs`, without changing the host feature guard.

Selection happens at the module boundary. Implementation files retain guards
for differences within a platform, such as unit-test locking, atomic-register
support, and optional scheduler timing.

The host backend does not execute scheduler threads as native OS threads.
Its synthetic stack frame models the
current embedded frame so thread-initialization tests run on the host.
Scheduler timer limits and initial stack requirements come from the selected
platform's trait constants.

Host test hooks use the same platform condition. They must remain available
when Cargo builds the library without `cfg(test)` for integration tests, so
`cfg(test)` alone cannot select the host backend.

Use `--features host` with an explicit native `--target` as described in
[the testing guide](testing.md). Embedded builds keep `host` disabled; enable
hardware features individually, such as `embedded-examples` and
`sched-isr-timing`, rather than using `--all-features`.

The shared API still uses a 32-bit initial thread context, and
`CycleCounterPort` accepts Cortex-M DCB/DWT peripherals. Separating the
implementations does not remove those remaining porting constraints.

The common trait types are re-exported from the crate root together with
`InitialThreadContext`, `CortexMPlatform`, `HostPlatform`, `DefaultPlatform`,
and the public platform helpers such as `spawn_main_thread`,
`init_dwt_cycle_counter`, `dwt_cycle_count`, `get_elapse_cycles`, and
`get_elapse_msec`.
