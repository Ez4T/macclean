# macclean

A terminal app that surfaces what's eating your disk, **classifies each item by
safety class** with the evidence behind it, and reclaims space only after you
confirm. Both a visual disk map and a classifier — it never deletes without a
Confirm.

See [`CONTEXT.md`](./CONTEXT.md) for the vocabulary and [`docs/adr/`](./docs/adr)
for the decisions behind the design.

> [!WARNING]
> **Use entirely at your own risk.** macclean reclaims space by **deleting files**.
> It is a personal research project provided **as-is, with no warranty** — the author
> does **not** guarantee it is safe and accepts **no liability** if it deletes data you
> wanted or otherwise harms your device. There is **no support** and **no commercial
> use**. See [Disclaimer](#disclaimer) and [License](#license) before running it.

## Install

No Rust, no Homebrew, no sudo — one line on any Mac:

```sh
curl -fsSL https://ez4t.github.io/macclean/install.sh | sh
```

This installs a single universal binary to `~/.local/bin` and adds it to your
`PATH` (restart your shell or run `exec zsh` afterward). The binary is unsigned
but installs without a Gatekeeper prompt — see
[`docs/adr/0007`](./docs/adr/0007-public-install-via-curl-sh.md) for why.

## Run

```sh
# Launch the TUI over $HOME (default root)
macclean

# Non-interactive: print the classified scan
macclean scan --root ~/Documents --min-unclassified-mb 200

# Whole disk (may need elevated access)
macclean --full-disk
```

The TUI opens immediately with a scan screen while filesystem sizing runs in the
background.

In the TUI: `↑/↓` move · `Space` tick the highlighted Item or group into a
multi-selection · `a` select/clear all reclaimable Items scan-wide · `o` override
an Unclassified item · `t` toggle the on-disk-size overview pane · `c` Confirm
reclaim (the selection if one is built, else the highlighted group/Item) · `q`
quit. While a Reclaim is running, `s` or `Esc` stops before the next Item starts.

## From source

```sh
cargo build --release
./target/release/macclean
```

## Uninstall

```sh
rm ~/.local/bin/macclean
# then remove the `export PATH="$HOME/.local/bin:$PATH"` line the installer added to ~/.zshrc
```

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
| `dedup.rs` | Post-classification Redundant Copy detection (size pre-filter + blake3) |
| `reclaim.rs` | Hybrid clean + destination-by-class (ADR-0002, ADR-0004) |
| `tui.rs` | Enriched navigable list, on-disk-size overview pane, and async Reclaim progress (ratatui) |
| `update.rs` | Fail-soft update check via curl against `releases/latest` (ADR-0009) |

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

The Scan → Classify → Reclaim pipeline is functional end-to-end, including
post-classification Redundant Copy detection (`dedup.rs`: non-Reclaimable Items
are size-prefiltered and confirmed by blake3, run as a second pass over the Scan).
The TUI offers an enriched navigable list, a toggleable on-disk-size overview pane
(`t`), and the deliberate Confirm gate; the Ruleset supports a rich, composable
set of match kinds (globs and AND/OR combinators). Every roadmap item named so far
is implemented and covered by the test suite (`cargo test`).

## Disclaimer

macclean is a **personal research project**, built and shared by the author for
their own learning. It is **not a product**. By using it you accept the following:

- **No warranty, no guarantee of safety.** The software is provided *as-is*. It
  reclaims space by **deleting files**, and the author does **not** guarantee it
  will not delete data you wanted, corrupt files, or otherwise harm your device.
  You use it **entirely at your own risk**, and the author is **not liable** for
  any loss or damage of any kind. Back up anything you cannot afford to lose.
- **No support.** No feature is officially supported. There is **no commitment**
  to maintain it, fix bugs, answer questions, accept contributions, or respond to
  issues. It may change or stop working at any time.
- **No data collection or retention.** macclean runs **entirely on your machine**.
  It makes **no network calls**, includes **no telemetry or analytics**, and
  **collects, transmits, and retains no personal data** — nothing ever leaves your
  device.
- **Non-commercial only.** It is provided for personal, research, and other
  non-commercial purposes. **Commercial use is not permitted** — see [License](#license).

## License

Licensed under the **[PolyForm Noncommercial License 1.0.0](./LICENSE)**. You may
use, study, modify, and share macclean for **non-commercial** purposes; **commercial
use is not permitted**. This is a *source-available* license — not an OSI-approved
"open source" license, because it restricts commercial use. The full warranty and
liability disclaimer in the [LICENSE](./LICENSE) governs your use.
