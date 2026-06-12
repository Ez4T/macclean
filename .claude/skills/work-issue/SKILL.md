---
name: work-issue
description: Resume development on the macclean disk-cleaner by implementing a GitHub issue end-to-end, honoring the project's glossary (CONTEXT.md) and decisions (docs/adr). Use when the user says "work on issue N", "implement the next macclean task", "continue macclean", "grab an issue", or names an issue number in this repo.
---

<context>

This repo is **macclean** (GitHub: Ez4T/macclean) — a Rust + ratatui TUI that
scans disk usage, classifies each item by a **Safety Class**, and reclaims space
only after an explicit Confirm. It is both a visual disk map and a classifier;
nothing is ever deleted without confirmation.

Before writing any code, read these so your work stays consistent with the
established design:

- **CONTEXT.md** — the domain glossary. Use these exact terms: Scan, Item,
  Safety Class (Regenerable, Reinstallable, Cache, Redundant Copy, Irreplaceable,
  Unclassified), Reclaimable, Protected, Evidence, Recovery Method, Reclaim,
  Confirm, Rule, Ruleset.
- **docs/adr/0001–0006** — the load-bearing decisions:
  - 0001 fail-safe: unrecognized → Unclassified, never auto-offered
  - 0002 hybrid reclaim: canonical clean tool (`cargo clean`…) else `rm`
  - 0003 data-driven ruleset (rules are data, not code)
  - 0004 reclaim destination by class (Reclaimable → permanent; overridden
    Unclassified → Trash)
  - 0005 Rust + ratatui stack
  - 0006 measure actual on-disk size, not apparent length
- **README.md** — module map and current status.

Module map: `model.rs` (domain types) · `scan.rs` (on-disk sizing) ·
`ruleset.rs` (rules + defaults) · `classify.rs` (applies ruleset, Unclassified
fallback) · `reclaim.rs` (hybrid clean + destination) · `tui.rs` (ratatui UI).

</context>

<what-to-do>

1. Identify the target issue. If the user gave a number, use it; otherwise run
   `gh issue list` and prefer the lowest-numbered P1. Read it fully with
   `gh issue view <N>` — implement to its acceptance criteria.
2. Create a branch (`git checkout -b issue-<N>-short-slug`). Never commit
   straight to `main` unless the user says so.
3. Implement, keeping identifiers aligned with CONTEXT.md and honoring every
   relevant ADR. If a change would contradict an ADR, stop and flag it rather
   than silently deviating.
4. Verify before claiming done:
   - `cargo build` is clean.
   - Add/run tests where the issue calls for them (`cargo test`).
   - Smoke-test against a temp fixture, e.g.
     `cargo run -- scan --root /tmp/<fixture> --min-unclassified-mb 1`.
     Build the fixture so the behavior under test actually triggers.
5. Commit atomically with a message that references the issue, then open a PR
   (or commit to `main` if the user requested it) and close the issue
   (`gh issue close <N>` or `Closes #<N>` in the PR).

</what-to-do>

<guardrails>

- **Do NOT run macclean against the real `$HOME`** until issue #1 (the
  Irreplaceable/Protected guardrail) is implemented. Use temp fixtures.
- Reclaim must never delete a Protected item, nor an Unclassified item without an
  explicit override — preserve these guardrails in any change.
- Priority order: #1 and #2 (the P1s) make the Protected guardrail real and add
  Redundant Copy detection — do them before the P2/P3 polish.
- End commit messages with:
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`

</guardrails>
