# Host tests

[Documentation index](../README.md)

The scheduler data-structure tests run on the native host target. From the
workspace root, use:

```sh
cargo test --manifest-path rtsched/Cargo.toml --target x86_64-unknown-linux-gnu
```

The explicit target matters for board-oriented workspaces because Cargo may
otherwise inherit an embedded default target, which cannot build Rust's host
test harness. On a non-x86 Linux host, replace `x86_64-unknown-linux-gnu` with
the `host:` value from `rustc -vV`.
