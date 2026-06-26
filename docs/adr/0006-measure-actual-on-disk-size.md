# Scan measures actual on-disk size, not apparent file size

Every size the Scan reports is the actual allocated on-disk size (blocks consumed),
not the apparent file length. We chose this because sparse files and APFS clones
make apparent size badly misleading for exactly the largest Items — a VM disk image
can report 39 GB apparent while occupying far less on disk — and an app whose entire
job is "how much will I get back" must not overstate what reclaiming frees.

## Consequences

- Sizing is slower and more code than reading `st_size`; the Scan must query
  allocated blocks and account for cloned/shared blocks.
- Reported numbers will sometimes be smaller than what Finder or naive `du` shows;
  this is correct and should be explained where it surprises the user.
- The default Scan root is `$HOME` with no sudo (full-disk is an explicit opt-in),
  so system paths needing elevated access are out of scope unless requested.

## Exception: matched directories skip the full recursive walk

When a directory matches a Rule during a Scan, the walk prunes recursion into
that subtree entirely — neither rule matching nor fd opening happens for any
descendant. This avoids the O(N_files × N_rules) `fstatat` overhead that makes
$HOME scans slow when directories like `node_modules` contain hundreds of
thousands of files.

Size for the pruned subtree is obtained in two steps:

1. **`getattrlistat(ATTR_DIR_ALLOCSIZE)`** — a single syscall that returns the
   total on-disk allocated size of the directory tree. Supported on HFS+ and
   similar filesystems.
2. **Rule-free `jwalk` byte count** — a parallel recursive walk with no rule
   matching, used as the fallback on filesystems (e.g. APFS) where
   `ATTR_DIR_ALLOCSIZE` returns 0. This still uses allocated-block sizing
   (`st_blocks × 512`) to remain consistent with ADR-0006.

In both cases the size is derived from `st_blocks`, so it remains consistent
with the allocated-blocks policy. The performance win (avoiding rule matching
for every file in a matched subtree) applies regardless of which path executes.
