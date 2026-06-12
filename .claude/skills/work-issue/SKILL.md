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

1. Work exactly **one** issue per invocation. Identify the target: if the user
   gave a number, use it; otherwise run `gh issue list` and prefer the
   lowest-numbered P1. **If there are no open issues, do not branch or change
   anything — report "All issues done" and stop (this ends any loop).** Read the
   chosen issue fully with `gh issue view <N>` and implement to its acceptance
   criteria.
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
5. Commit atomically with a message that references the issue, push the branch,
   and open a PR whose body contains `Closes #<N>`. Then **auto-merge it**:
   - `cargo build` and `cargo test` MUST be green first — this is the merge gate.
     If either fails, leave the PR open, comment why, and stop. Do not merge.
   - Merge with `gh pr merge <PR> --squash --delete-branch`. The `Closes #<N>`
     trailer closes the issue on merge.
   - `git checkout main && git pull` so the next iteration starts from the merged
     state.

</what-to-do>

<afk-loop>

This skill does **one** issue per run, so it composes with `/loop` for AFK
operation. To drain the backlog unattended, the user runs:

```
/loop use the work-issue skill to implement the next open issue, PR it, and auto-merge
```

Self-paced (no interval) is correct: finish one issue, then continue to the next,
and **stop when step 1 finds no open issues**. Between iterations, always start
from a clean, up-to-date `main`. If an iteration is blocked (failing gate,
ambiguous acceptance criteria, or a change that would contradict an ADR), stop the
loop and surface it rather than guessing or merging broken code.

</afk-loop>

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
