# Changelog

## [0.2.0] - 2026-11-14

This release adds synchronization and message queues, makes thread lifecycle
states explicit, and separates the native test backend from the Cortex-M port.
It contains breaking API changes from v0.1.0.

### Added

- Allocation-free `BinarySemaphore`, `CountingSemaphore`, and `Mutex<T>` with
  an RAII `MutexGuard`. Waiters are ordered by scheduler deadline; contended
  mutexes can boost their owner's scheduler deadline.
- Bounded `MessageQueue<T, N>` with copied FIFO messages, blocking and
  nonblocking send/receive, millisecond timeouts, and per-thread cancellation.
- `ThreadHandle`, `ThreadId`, `current_thread()`, and `current_thread_id()` for
  inspecting scheduler threads without passing raw context pointers.
- An explicit `host` feature for native data-structure tests and documentation
  builds. This backend does not execute scheduler threads or hardware context
  switches.
- Optional SysTick-to-PendSV cycle diagnostics through `sched-isr-timing`.
- Synchronization and message-queue examples, expanded host regression tests,
  and guides for scheduling, platform integration, waiting, and error handling.

### Changed and breaking changes

- Thread builders return `ThreadHandle` instead of `*mut ThreadCtx`.
  `CfsThreadBuilder::try_spawn` and `RtThreadBuilder::try_spawn` were removed;
  `spawn` validates setup and panics on invalid arguments before writing storage.
- The idle fallback uses dedicated `IdleThread` storage. `spawn_main_thread`
  now initializes and registers it from a name, entry function, thread storage,
  and stack storage. `register_idle_thread` accepts a `ThreadHandle` to an
  `IdleThread`, rather than a raw pointer to a CFS thread.
- `ThreadState::Ready` is replaced by `ReadyActive`. `ReadyInactive` represents
  an RT thread awaiting its next periodic release, `Waiting` represents a finite
  wait, and `Suspended` represents an indefinite wait. Use `is_blocked()` to
  recognize both wait states.
- `ThreadCtx::is_cfs` is replaced by `kind: ThreadKind`, with `is_cfs()`,
  `is_rt()`, and `is_idle()` accessors. `SchedInfo` is renamed `CfsSchedInfo`.
- `CfsThread::wait_info()` and `RtThread::wait_info()` return
  `(u32, Option<SyncType>)` instead of `(u32, u32)`. A timer sleep has no sync tag.
- RT job completion calculates the next release and resets runtime in
  `yieldyi()`. `set_rt_thread_start_time` was removed; applications no longer
  reset runtime at job entry. Sleeping within a job preserves its runtime.
- `RtKTimer::init_rt_thread`, raw RT timer accessors, and the public
  `dequeue_cfs_thread_to_waitq`, `dequeue_rt_thread_to_waitq`, and
  `enqueue_rt_thread_from_waitq` helpers were removed from the application API.
  Builders initialize RT timers, and sleep, synchronization, and message-queue
  operations manage wait transitions.
- Active timers and inactive release/wake timers use separate trees with cached
  minima. Finite waits use an ordered timeout tree; indefinite waits use an
  intrusive FIFO list.
- Native tests and documentation builds must explicitly enable `host` and
  select a native target. Unsupported hardware targets fail during the build.

### Fixed

- `yieldyi()` and `msleepyi()` check for a selected, running thread and an
  available context switch before changing scheduler state. Invalid non-idle
  calls panic in both debug and release builds; idle-thread calls remain no-ops.
- PendSV saves/restores FP registers only for hard-float (`eabihf`) targets,
  allowing non-FPU Cortex-M builds to assemble successfully.
- Blocking synchronization and message-queue calls reject interrupt-handler,
  masked-interrupt, and other invalid waiting contexts without changing state.
- Timer accounting keeps a reload epoch so a cooperative yield cannot charge
  the same elapsed ticks twice. Message-queue retries retain their original
  absolute timeout, and timeout/cancellation removes both waiter memberships.
- CI example builds explicitly use the firmware linker script so the produced
  ELF retains the scheduler's exception handlers.

### Supported targets

- `thumbv7m-none-eabi` (Cortex-M3)
- `thumbv7em-none-eabi` and `thumbv7em-none-eabihf` (Cortex-M4/M7)
- `thumbv8m.main-none-eabi` and `thumbv8m.main-none-eabihf` (ARMv8-M Mainline)

Soft-float builds must not enable FP use. ARMv6-M, ARMv8-M Baseline, other ARM
profiles, and custom target specifications require a separate port. See the
[platform guide](docs/arch.md). The minimum Rust version remains 1.85, with
edition 2024.

### Migrating from v0.1.0

1. Replace raw thread-pointer variables with the `ThreadHandle` returned by
   `spawn`. Use its inspection methods and `id().as_u32()` when a numeric ID is
   needed; retain the board-owned thread, stack, and timer storage for all
   scheduler use.
2. Replace the old CFS idle thread with `MaybeUninit<IdleThread>` storage and
   call `spawn_main_thread(name, entry, thread_storage, stack_storage)` after
   timer and scheduler setup. The [embedded examples](docs/examples.md) show
   the complete startup sequence.
3. Update state matches, `SchedInfo` imports, `ThreadCtx::is_cfs` field reads,
   and `wait_info()` consumers to the types described above. Remove manual RT
   runtime resets and direct queue-transition calls.
4. Call `yieldyi()` and `msleepyi()` only from running threads with context
   switching enabled. The [error-handling guide](docs/error_handling.md)
   documents the calling-context rules for all blocking APIs.
5. For native tests, use an explicit host target and feature, for example:
   `cargo test --features host --target x86_64-unknown-linux-gnu`.

[0.2.0]: https://github.com/chungae9ri/rtsched/compare/v0.1.0...v0.2.0
