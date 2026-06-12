# macclean

A terminal app that surfaces what's eating your disk, **classifies each item by
safety class** with the evidence behind it, and reclaims space only after you
confirm. Both a visual disk map and a classifier — it never deletes without a
Confirm.

See [`CONTEXT.md`](./CONTEXT.md) for the vocabulary and [`docs/adr/`](./docs/adr)
for the decisions behind the design.

## Build & run

```sh
cargo build --release

# Launch the TUI over $HOME (default root)
./target/release/macclean

# Non-interactive: print the classified scan
./target/release/macclean scan --root ~/Documents --min-unclassified-mb 200

# Whole disk (may need elevated access)
./target/release/macclean --full-disk
```

In the TUI: `↑/↓` move · `o` override an Unclassified item · `t` toggle the
on-disk-size overview pane · `c` Confirm reclaim · `q` quit.

The overview pane (`t`) shows a proportional bar per Item — a 1-D "block treemap"
scaled so the largest on-disk Item fills the column, colour-coded by Safety Class.
A true 2-D squarified treemap was spiked and dropped as illegible in a narrow
terminal column (issue #7); the sorted-bar form is what stays readable.

## Safety classes

| Class | Reclaim behavior |
|---|---|
| Regenerable | Runs the canonical clean tool (`cargo clean`…), else `rm` (ADR-0002) |
| Reinstallable | `rm` (restored by the package manager) |
| Cache | `rm` (refilled automatically) |
| Redundant Copy | `rm` (the original survives) |
| Irreplaceable | **Protected** — never offered |
| Unclassified | Surfaced but not offered; override → moved to **Trash** (ADR-0004) |

## Module layout

| Module | Responsibility |
|---|---|
| `model.rs` | Core domain types (`SafetyClass`, `Item`, `Scan`) — mirrors `CONTEXT.md` |
| `scan.rs` | Filesystem measurement; **actual on-disk size**, not apparent (ADR-0006) |
| `ruleset.rs` | Data-driven `Rule`/`Ruleset` + curated defaults (ADR-0003) |
| `classify.rs` | Applies the ruleset; fail-safe to `Unclassified` (ADR-0001) |
| `reclaim.rs` | Hybrid clean + destination-by-class (ADR-0002, ADR-0004) |
| `tui.rs` | Enriched navigable list (ratatui) |

## Extending the ruleset

Drop a TOML file at `~/.config/macclean/rules.toml`; its rules are evaluated
before the bundled defaults. Example:

```toml
[[rules]]
name = "gradle-build"
class = "Regenerable"
clean_command = "./gradlew clean"
recover_command = "./gradlew build"
evidence = "build.gradle sits beside this build/ — Gradle build output."

[rules.matches]
DirBesideSibling = { dir = "build", sibling = "build.gradle" }
```

### Match kinds

A Rule's `matches` is one of these declarative, serializable conditions:

| Kind | Matches |
|---|---|
| `DirNamed { dir }` | a directory with this name |
| `DirBesideSibling { dir, sibling }` | a directory named `dir` next to a file `sibling` |
| `DirContainingFile { file }` | a directory directly containing this marker file |
| `PathSuffix { suffix }` | any Item whose full path ends with this string |
| `NameSuffix { suffix }` | any Item whose name ends with this string (fixed extension) |
| `NameGlob { pattern }` | any Item whose name matches a `*`/`?` glob (e.g. `*.zst`) |
| `All { of = [ … ] }` | every nested condition matches (logical AND) |
| `Any { of = [ … ] }` | any nested condition matches (logical OR) |

The last three are the richer conditions added for single-file items and
composition. Combinators nest, so a Rule can require a glob *and* a marker:

```toml
[[rules]]
name = "zstd-or-zstandard"
class = "RedundantCopy"
evidence = "A zstd archive recognized by extension."
matches.Any.of = [
  { NameGlob = { pattern = "*.zst" } },
  { NameGlob = { pattern = "*.zstd" } },
]
```

## Status

Scaffold. The Scan → Classify → Reclaim pipeline and the TUI are functional, with
a toggleable on-disk-size overview pane (`t`) and a rich, composable set of Rule
match kinds (globs and AND/OR combinators). Not yet built: Redundant Copy
detection (blake3 checksum comparison — the `trash` crate and `blake3` dep are
wired in). This is the natural next step and is not blocked by the decisions above.
