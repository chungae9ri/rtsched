# Wait queue

[Documentation index](../README.md) · Source: [`src/waitq.rs`](../src/waitq.rs)

The wait queue tracks blocked [threads](thread.md#thread-states). `msleepyi()`
blocks a thread until its sleep deadline; synchronization and
[message queue](message_queue.md) operations can suspend a thread until a resource
is ready. Blocked threads return to `Ready` before they can run again.

Finite waits use the ordered timeout tree; indefinite waits (`wake_at == u64::MAX`)
use an intrusive FIFO list with constant-time insertion and removal. Per-object
waiter urgency ordering is unchanged. Wait diagnostics visit finite waits first,
then suspended threads in registration order. The shared wait timer remains
linked when its earliest timeout is unchanged, while ready-timer selection is
still refreshed after block/wake transitions.
