# Mac Clean

A terminal app (TUI) that surfaces what is consuming disk space and labels each
item with a **Safety Class** plus the evidence behind that label, so a developer
can reclaim space confidently. It is both a visual disk map and a classifier;
nothing is ever deleted without explicit confirmation.

## Language

**Scan**:
A single pass over the filesystem that produces the usage map and assigns a
**Safety Class** to every **Item** it surfaces.
_Avoid_: analyze, index, crawl

**Item**:
A file or directory the **Scan** surfaces as a unit the user can act on (e.g. a
single `target/` directory, one `node_modules`, one duplicate tree).
_Avoid_: entry, node, target, candidate

**Safety Class**:
The label assigned to each **Item** describing what it is and the cost of
deleting it. Exactly seven values. The first four are **Reclaimable** (the app
may offer them). Irreplaceable is **Protected**. Unclassified and BrowserCache
are surfaced but never auto-offered — both require an explicit override.

- **Regenerable** — build output that a build command recreates (e.g. `cargo
  clean` then `cargo build`, `flutter clean` then rebuild). Recovered by
  *rebuilding*.
- **Reinstallable** — dependencies a package manager restores (e.g.
  `node_modules` via `npm install`). Recovered by *reinstalling*. Kept distinct
  from Regenerable because the recovery action differs.
- **Cache** — data automatically refilled on next use, with no command needed
  (e.g. `~/.npm`, `~/.cache/uv`). Recovered by *doing nothing*.
- **BrowserCache** — a browser's on-disk cache (Safari, Chrome, Firefox, Arc,
  Brave, Edge). Browsers rebuild it on next use, but clearing it has a real
  perceived cost (slow page loads, video rebuffering). Not offered for Reclaim
  by default; the user must explicitly override each Item first.
- **Redundant Copy** — a byte-identical duplicate of data that exists elsewhere;
  deleting it loses nothing because the original survives (e.g. a `-backup`
  tree, a `.zst` of an existing image). Recovered by *the surviving copy*.
- **Irreplaceable** — real, non-recoverable data (databases, VM volumes, source
  code, documents). **Protected**: never offered for deletion.
- **Unclassified** — a large **Item** the classifier has no rule for. Surfaced
  prominently so it is never *hidden*, but not **Reclaimable** by default: the
  user must inspect and explicitly override before it can be **Reclaimed**. The
  fail-safe default for everything unrecognized.

_Avoid_: category, type, status

**Reclaimable**:
An **Item** the app may offer to delete: the four classes Regenerable,
Reinstallable, Cache, and Redundant Copy. Irreplaceable is never Reclaimable;
Unclassified is not Reclaimable until manually overridden.
_Avoid_: deletable, junk, removable

**Protected**:
The guardrail state of an Irreplaceable **Item**: the app actively refuses to
offer it for deletion. Protection is a property of the Item, enforced by the
classifier, not a user setting.
_Avoid_: locked, excluded, ignored

**Evidence**:
The concrete proof the **Scan** used to assign a **Safety Class** — e.g. a build
manifest beside the directory, a checksum match against another tree, a recent
git commit. Always shown alongside the class so the user can trust or override
it.
_Avoid_: reason, signal, heuristic

**Recovery Method**:
How a deleted **Reclaimable** Item is restored: rebuild command, reinstall
command, automatic refill, or "the surviving copy". Each Reclaimable Safety
Class implies exactly one.
_Avoid_: undo, restore path

**Reclaim**:
To delete a **Reclaimable** Item in order to free space — only ever after an
explicit **Confirm**.
_Avoid_: clean, purge, remove, free

**Confirm**:
The explicit user approval required before any **Reclaim**. There is no path
from Scan to deletion that skips it.
_Avoid_: approve, accept, ok

**Rule**:
A single declarative entry the classifier evaluates: a match condition (e.g. "a
`target/` directory beside a `Cargo.toml`") mapped to a **Safety Class** and, for
Regenerable and Reinstallable, the clean command that defines its **Recovery
Method**. Rules are data, not code.
_Avoid_: detector, matcher, pattern, heuristic

**Ruleset**:
The collection of **Rule**s the classifier applies. Composed of the curated
**default ruleset** shipped with the app and the user's own added or overriding
rules. An **Item** matching no Rule becomes **Unclassified**.
_Avoid_: config, rules engine

## Example dialogue

> **Dev:** The scan found a 10 GB `target/` under gto-solver. What class?
> **App:** Regenerable. Evidence: there's a `Cargo.toml` next to it, so it's
> Rust build output. Recovery Method is rebuild — `cargo build` restores it.
> **Dev:** And the OrbStack `data.img.raw`, that's 39 GB?
> **App:** Irreplaceable, so it's Protected — I won't offer it for deletion.
> Evidence: it's a VM volume, not regenerable from any command. Shrink it from
> inside OrbStack instead.
> **Dev:** The `antigravity-backup` tree?
> **App:** Redundant Copy. Evidence: every file checksums identically to
> `antigravity`, which is newer. Reclaiming it loses nothing — the surviving
> copy is the original.
> **Dev:** Reclaim the target and the backup, leave OrbStack.
> **App:** That needs a Confirm. Reclaim these two for ~13 GB? [y/N]
