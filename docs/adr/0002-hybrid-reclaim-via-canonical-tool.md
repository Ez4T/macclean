# Reclaim runs the canonical clean tool, falling back to rm

For Regenerable and Reinstallable Items, Reclaim prefers the toolchain's own
clean command (`cargo clean`, `flutter clean`, …) and only falls back to a direct
`rm` of the identified directory when no such command exists (e.g. `node_modules`)
or the toolchain is not installed. We chose this because the owning tool knows its
artifacts better than a path heuristic does — in practice `cargo clean` reclaimed
10.8 GB where `du` showed only 6.3 GB for `target/`, and it removes exactly what it
owns without ever touching source.

## Consequences

- Each Regenerable rule must carry its clean command, not just a path pattern.
- Reclaim behavior depends on the environment: the same Item may be cleaned via
  tool on one machine and via `rm` on another where the toolchain is absent. This
  is surfaced to the user, not hidden.
- The app shells out to external developer tools, accepting that dependency rather
  than reimplementing each tool's cleanup logic.
