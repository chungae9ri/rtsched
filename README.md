# rtsched

`rtsched` is a small `no_std` runtime scheduler for single-core embedded systems.
Its platform traits are designed to support ports to different architectures;
Cortex-M is the current implementation. It combines soft real-time scheduling
with fair background scheduling.

The hardware port supports ARMv7-M, ARMv7E-M, and ARMv8-M Mainline, with separate
soft-float and hard-float context-switch paths. See the
[supported target list](docs/arch.md).

The crate provides threads, timers, sleeping and waiting, synchronization,
message queues, and diagnostics. Idle-thread support lets applications save
power when no work is runnable.

Board crates own hardware setup and thread and stack storage. See
[`rtsched-boards`](https://github.com/chungae9ri/rtsched-boards) for example
integrations.

## Documentation

- [Release notes and migration from v0.1.0](CHANGELOG.md)
- [Scheduling](docs/sched.md), [kernel timers](docs/ktimer.md), and
  [CFS run queues](docs/runq.md)
- [Threads](docs/thread.md) and [wait queues](docs/waitq.md)
- [Message queues](docs/message_queue.md)
- [Platform integration](docs/arch.md)
- [Diagnostics](docs/diagnostics.md)
- [Embedded examples](docs/examples.md) and [host tests](docs/testing.md)
- [Error handling](docs/error_handling.md)
