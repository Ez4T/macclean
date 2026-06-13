# Scan uses an fd-relative walker

The Scan walker keeps each directory open and measures children with
`fstatat(AT_SYMLINK_NOFOLLOW)` relative to that directory fd, opening child
directories with `openat`. This supersedes the ADR-0005 implementation detail
that used `jwalk` for the Scan path, while preserving the Rust + ratatui stack
decision itself.

We chose this because deep cache trees made full-path metadata calls pay kernel
path resolution once per path component, per Item. Measured on
`~/Library/Caches`, the old path-based stat path spent far more kernel CPU than
`find`, whose traversal keeps directory fds and stats leaf names relative to
those fds. A wider parallel walk cannot remove that per-stat O(depth) cost.

The walker still emits a strict depth-first stream of per-entry facts for the
classifier fold: own on-disk blocks, directory/file kind, and the first matching
Rule index. Rule probes that need sibling or child Evidence use fd-relative
`fstatat` as well, so marker checks do not reintroduce full absolute path stats.

## Consequences

- Scan performance in deep trees depends on entry count, not absolute path depth.
- The Scan path is Unix/macOS-specific and uses thin `libc` calls for
  `openat`, `fdopendir`, `readdir`, and `fstatat`.
- Child directory fds are opened in bounded batches so a wide directory does not
  exhaust the process file descriptor limit before recursion starts.
- `jwalk` remains available for Redundant Copy fingerprint traversal, but it is
  no longer the Scan walker.
