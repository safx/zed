# Agentium crate rules

## Binary entry point must handle `--printenv`

The `util::shell_env` module launches the current executable with `--printenv` to capture shell environment variables when creating terminals. Without handling this flag in `main()`, a full GUI app launches instead of printing env vars and exiting. This causes a phantom second window on every terminal creation (including pane splits).

```rust
fn main() {
    if std::env::args().any(|arg| arg == "--printenv") {
        util::shell_env::print_env();
        return;
    }
    // ... rest of app
}
```

Any new binary in the Zed repo that creates terminals needs this same pattern (see `crates/zed/src/main.rs`).
