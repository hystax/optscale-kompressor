# Cursor hook events, CLI 2026.09.26-dd393fe

Shapes from the spikes of 2026-11-25 (docs/design/CURSOR.md §12, Appendix B),
with identifiers, paths and the e-mail address replaced. The MCP text result is
a synthetic build log of the size the spikes used (about 30 KB, under Cursor's
40 KB spill threshold); the rest are the captured shapes as trimmed in
Appendix B. `stop-desktop.json` is the desktop's `stop` event, which the CLI
does not fire under `--print`.

Tier 0 of the plan (§13.2) replaces these with raw captures from the current
CLI and desktop builds, in a directory named after each build, so a build that
changes a payload shows up as a new directory and a diff.
