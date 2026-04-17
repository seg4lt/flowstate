---
name: flowzen-diagnostics
description: Capture flowstate runtime diagnostics — inspect the active session's state, provider enablement, in-flight turns, and recent daemon log lines so issues can be reproduced and reported.
argument-hint: "[--provider <kind>] [--lines <n>]"
---

# Flowzen Diagnostics

Fixture skill used to exercise the disk scanner in
`crates/core/provider-api/src/skills_disk.rs`. When a session opens
with this directory as its `cwd`, the slash-command popup should show
`/flowzen-diagnostics` with a "project" badge and the argument hint
rendered inline.

## Body

The scanner only reads the YAML frontmatter — this section is
included to make the fixture a realistic `SKILL.md` rather than a
bare frontmatter block.

Run `cargo test -p zenui-provider-api --lib skills_disk` to exercise
the parser unit tests.
