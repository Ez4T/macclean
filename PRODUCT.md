# Product

## Register

product

## Users

Developers using a Mac who need to understand what is consuming disk space and reclaim safe-to-rebuild or safe-to-restore Items without guessing. They are usually in a terminal, likely under storage pressure, and need the interface to make risk obvious before any destructive action.

## Product Purpose

macclean surfaces disk usage as classified Items, explains the Evidence and Recovery Method for each Safety Class, and reclaims space only after an explicit Confirm. Success means users can quickly identify large reclaimable groups, inspect the proof behind them, and avoid deleting protected or unknown data by accident.

## Brand Personality

Cautious, explicit, technical. The product should feel like a trustworthy local tool: dense enough for repeated terminal use, plain-spoken about risk, and direct about what will happen.

## Anti-references

Avoid one-click cleaner apps, scareware dashboards, decorative analytics, and any interface that hides deletion risk behind vague labels like junk or optimize. Avoid workflows that make Protected or Unclassified data feel like normal reclaim targets.

## Design Principles

- Safety class first: organize decisions around recoverability, not raw file paths.
- Evidence beside action: every reclaim choice should keep its proof and Recovery Method close.
- Batch with guardrails: grouped actions are useful only when Confirm makes the full scope explicit.
- Terminal density without clutter: show totals, counts, and paths in a scan-friendly hierarchy.
- Conservative by default: Protected is never offered, and Unclassified requires deliberate override.

## Accessibility & Inclusion

Keep the TUI fully keyboard-operable. Do not rely on color alone for Safety Class meaning; labels and totals must remain visible in monochrome terminals. Preserve readable contrast for badges, selected rows, status text, and Confirm prompts. Respect the user flow in reduced or no-animation terminal environments.
