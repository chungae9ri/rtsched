# Scheduler tracing

[Documentation index](../README.md) · Source: [`src/diagnostics`](../src/diagnostics)

Tracing hooks are available for scheduler event counters and optional callbacks:

- `trace_counters()` returns saturating counters for context switches, RT
  deadline misses, wakeups, and cooperative yields.
- `reset_trace_counters()` resets those counters to zero.
- `set_trace_fn(fn(TraceEvent))` registers a lightweight callback for each
  traced event, and `clear_trace_fn()` removes it.

Trace callbacks run from scheduler paths that may be inside a critical section
or interrupt-triggered context switch path, so callbacks should stay short,
non-blocking, and allocation-free.
