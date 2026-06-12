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
