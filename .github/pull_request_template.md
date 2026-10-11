# =============================================================================
# Pull request template
# =============================================================================

## What this changes

<!-- One or two sentences. What is different after this merges, and why it was
     worth doing. Not a list of files -- the diff is right there. -->

## Why this way

<!-- The decision, not the implementation. What were the alternatives, and what
     made this one better? If a comment in the code explains it, link to the
     line rather than repeating it here.

     This section is the one reviewers actually read, and the one most often
     skipped. A PR with an empty "why" is usually a PR nobody understands
     either. -->

## How it was verified

<!-- Be specific about what was run, not just that it "passes".

     - [ ] `cargo test --workspace`
     - [ ] `cargo clippy --workspace --all-targets -- -D warnings`
     - [ ] `cargo fmt --all -- --check`
     - [ ] `./tools/loom-check.sh`  — only if this touches `crates/telemetry`
     - [ ] the thing that proves *this particular change*, by name

     The last box is the one that matters. The first three are enforced by CI
     and need no claim. -->

## What a reviewer should check

<!-- Point at the specific places where a mistake would be expensive and is not
     caught by a test. This is where you tell someone "look hard at the
     comparison here, because if it is inverted the bot trades at the wrong
     price and every test still passes". -->

<!-- A good answer names a line. A bad answer says "the logic". -->

## Risk

<!-- Pick one and say why:

     [ ] None -- no production behaviour changes
     [ ] Behaviour changes, covered by a test that fails without this diff
     [ ] Behaviour changes, and I do not know how to test it   <- say so, and
                                                                   explain what
                                                                   you would do -->

     The third box is not a failure to fill this template in. Saying "I cannot
     test this" is the only honest answer when it is true, and it is much more
     useful than a test that asserts the change is correct because the author
     wrote it that way. -->

---

<!-- House notes, not instructions to the reviewer:

     * `panic = "abort"` in the release profile is harmless for tests: cargo
       forces `panic = "unwind"` onto test targets, which
       `test_profile_invariants.rs` verifies rather than assumes.
     * `#[cfg(not(loom))]` tests are skipped rather than silently passing. If a
       change touches `crates/telemetry/src`, the loom gate is the one that
       catches what `cargo test` cannot.
     * `FORBIDDEN`/`FIXME` is not available: `unsafe_code = "forbid"` is
       workspace-wide. -->