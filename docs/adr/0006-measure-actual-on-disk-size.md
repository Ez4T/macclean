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

## The matched-dir size attribute (issue #43)

There is no `ATTR_CMN_TOTALSIZE` on macOS. An early version of the matched-dir
fast path requested commonattr `0x400` believing it was a recursive total-size
attribute; `0x400` is in fact `ATTR_CMN_MODTIME`, so `getattrlistat` returned the
directory's modification time and the code read its `tv_sec` (~1.78 × 10⁹) as a
byte count. Every matched directory therefore reported ~1.7 GiB regardless of its
real contents, and summing hundreds of `node_modules`/`target` Items produced an
impossible reclaimable total (12.6T observed on a 460GB disk — issue #43).

The single-syscall path now uses **only** `ATTR_DIR_ALLOCSIZE` (dirattr `0x8`),
which is a genuine recursive allocated-size attribute. It works on HFS+ and
returns 0 on APFS, where step 2 above (`fast_tree_bytes`, the rule-free
allocated-block walk) supplies the size as already documented. Both paths derive
size from real allocated blocks, never from an unrelated attribute.

## APFS clones are sized by their private (freeable) bytes (issue #47)

Regular files are sized by the bytes **unique** to them — what deleting the file
actually frees — via `getattrlistat(ATTR_CMNEXT_PRIVATESIZE)` (forkattr `0x8`,
requested with `FSOPT_ATTR_CMN_EXTENDED | FSOPT_NOFOLLOW`). APFS clones share
underlying blocks via copy-on-write, so allocated-block sizing reported each clone
at its full logical allocation and a tool that `clonefile(2)`s a project per
workspace over-reported the truly-freeable bytes. Private-size sizing removes that
over-count.

**Why this didn't reintroduce the `O(N_files)` cost the matched-dir exception
avoids (issue #41):** that exception avoids `O(N_files × N_rules)` *rule matching*,
not the per-file stat. On APFS the scan already does one `fstatat` per file in both
the main walk and the matched-dir fallback (`ATTR_DIR_ALLOCSIZE` returns 0 on APFS,
and HFS+ — where the single-syscall path fires — has no `clonefile`). Clone
accuracy is therefore an extra single syscall per regular file, which is cheap and
fully parallel; a `$HOME` scan stays well within budget. No clone-heaviness gate is
needed, and both sizing paths share one helper (`entry_on_disk_bytes`).

**Graceful fallback, never a regression:** when `PRIVATESIZE` is unavailable (older
macOS, non-APFS, error) the entry falls back to its allocated-block size — exactly
today's number. A legitimate private size of `0` (a fully-shared clone) is kept,
distinguished from an absent attribute via the result buffer's length word, so such
files are not silently re-inflated to full allocation.

**Accepted limitation — safe under-count:** `PRIVATESIZE` is a per-file attribute,
so summing it over a subtree under-counts when two clones *inside the same subtree*
share blocks (those blocks are private to neither file, yet deleting the whole
subtree frees them). Under-counting is the safe direction for a tool that must
never overstate what reclaiming frees, so we accept it rather than tracking
`CLONEID` reference sets across the subtree. Blocks shared with a clone *outside*
the subtree are correctly excluded (deleting the subtree would not free them).

The `.treehouse` Rule (issue #43) additionally collapses Treehouse's per-PR
workspaces into one Item rather than thousands.
