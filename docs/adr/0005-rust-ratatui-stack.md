# Built in Rust with ratatui

The app is built in Rust: ratatui + crossterm for the TUI, a parallel directory
walker (jwalk / the `ignore` crate) for the Scan, blake3 for Redundant Copy
checksums, and the `trash` crate for the ADR-0004 Trash path. We chose Rust because
the Scan over a large home tree (hundreds of GB) is the performance-critical core,
single-binary distribution suits a personal CLI/TUI tool, and it matches the
existing Rust toolchain on this machine. Swift was rejected despite better macOS
integration because the chosen TUI form (not GUI) has no strong Swift story; Go +
Bubble Tea was a close runner-up.

## Consequences

- macOS-specific operations (Trash, possibly volume/sparse-file size queries) go
  through crates or thin FFI rather than native frameworks.
- The Rule schema (ADR-0003) and clean-command execution (ADR-0002) are implemented
  as Rust data structures and subprocess calls.
