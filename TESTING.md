# =============================================================================
# TESTING.md — how the tests in this repository are arranged, and why
# =============================================================================

This repository has a specific way of testing that is not obvious from reading
any individual file. This document is the map.

If you are adding a test and cannot tell where it belongs, read "Where does my
test go?" below before adding it to the wrong layer.

---

## The shape of it

```
                          L4  CI and scheduled gates
        ci.yml · concurrency.yml · coverage.yml · mutation.yml · supply-chain.yml
                              │
   ┌──────────────────────────┴───────────────────────────┐
   │                   L3  Composition                      │
   │   pipeline_e2e.rs — the stages wired together, seams  │
   │   leak_budget.rs  — bounded memory over remote input  │
   └────────┬───────────────────────┬─────────────────────┘
            │                       │
   ┌────────┴────────┐    ┌─────────┴──────────┐
   │  L2  Edges      │    │  L2  Network       │
   │  sandbox crate │    │  stream::logs      │
   │  LiteSVM, goldens│   │  wire shape, reconnection
   └────────┬────────┘    └─────────┬──────────┘
            │                       │
   ┌────────┴───────────────────────┴────────┐
   │            L1  The core                    │
   │  curve · pricing · risk · venue · config  │
   │  math_invariants · promotion_gate ·        │
   │  curve_geometry · decode_matrix ·          │
   │  risk_sandbox                              │
   └────────┬───────────────────────┬──────────┘
            │                       │
   ┌────────┴────────┐    ┌─────────┴──────────┐
   │  L0  Telemetry  │    │  L0  Foundation     │
   │  counter · ring │    │  build profile,     │
   │  histogram      │    │  MSRV, feature gates│
   │  loom_concurrency│    │                     │
   └─────────────────┘    └─────────────────────┘
```

Each layer can fail on its own, and each layer's failure means something
different. That is the entire organising principle: **a red test should tell you
which kind of problem you have before you read the assertion.**

---

## The layers, and what a failure in each means

### L0 — Telemetry (`crates/telemetry`)

Zero dependencies, `unsafe_code = "forbid"`, hot path measured in nanoseconds.

| File | Claim it defends |
|---|---|
| `histogram_properties.rs` | Five laws: conservation, monotonicity, extremality, central tendency, totality |
| `loom_concurrency.rs` | Every legal C11 interleaving, exhaustively |
| `test_profile_invariants.rs` | The build profile does not break the harness |

**A failure here is a measurement lying to you.** Not a wrong trade — a wrong
number you would have made the trade on. The `sum_ns` overflow found in the
first week was of exactly this kind: green tests, a mean larger than any
sample inside it.

loom runs as its own gate (`tools/loom-check.sh`) because `--cfg loom` replaces
the atomics in `src/`, and forgetting the flag produces a suite that compiles,
passes, and proves nothing.

### L1 — The core (`crates/core`)

| File | Claim |
|---|---|
| `math_invariants.rs` | Curve maths cannot manufacture profit |
| `decode_matrix.rs` | Zero false positives and zero false negatives over captured mainnet |
| `risk_sandbox.rs` | Every refusal has a reason that ends a position |
| `promotion_gate.rs` | The gate refuses what it exists to refuse, and only Live broadcasts |
| `curve_geometry.rs` | Progress never goes backwards and never wraps |

**A failure here is a wrong decision.** The distinction that matters: `risk_sandbox`
catches danger; `promotion_gate` catches misconfiguration; `math_invariants`
catches a number that costs money. They fail for unrelated reasons and you
should not merge a fix for one while ignoring another.

### L2 — Edges (`crates/sandbox`, `crates/core/src/stream`)

The sandbox runs a real Solana VM in-process. It works on a clean checkout: when
the real 10 MB pump.fun dump is absent it loads a synthesised SBPF program
instead, and says so on stderr.

`leak_budget.rs` covers the one piece of this layer that is driven by remote
input. The stream interns instruction names, and before it deduplicated, an
unrecognised name cost one permanent allocation per *occurrence* rather than per
*distinct* name.

**A failure here is a wrong assumption about the outside world** — a wire shape
that changed, a program that emits something unexpected, a memory cost that
grows where it should not.

### L3 — Composition (`crates/core/tests/pipeline_e2e.rs`)

Every other test constructs its inputs directly, so none of them traverse the
code *between* stages. This one wires risk, configuration, room and pricing
together and asserts on the seams.

**A failure here is a bug that only exists between the parts.** A stage that
emits an input the next stage cannot legally accept passes every L1 test.

---

## Where does my test go?

Ask: **what would this failure mean?**

- "A number the operator reads is wrong" → L0
- "The bot made a wrong decision" → L1
- "Something outside changed" → L2
- "Two correct stages compose into something wrong" → L3

If the honest answer is "it would be a number but I cannot say whose", it
belongs in the layer that owns that number, and the fix is to find out — a test
you cannot place is usually a test that is asserting two things at once.

---

## What is deliberately not tested, and why

### Coverage is measured but never gated

Coverage counts lines executed. It cannot see whether an assertion means
anything. A test that calls `record()` once and asserts `count() == 1` is 100%
of that function's lines and zero of its value. A threshold therefore rewards
tests that reach code over tests that check it, and the cheapest way to satisfy
one is a large file of trivial assertions that go green on the first run and
survive refactors without ever failing again.

The number is published because it is good at finding *untested areas*: 62%
with a chart showing `crates/xtask` at zero is actionable in a way that
"62% < 80%" is not. It is a map, not a verdict.

### Mutation testing is not gated either

A survival rate is a trend. Zero survivors usually means only trivial code was
tested; a sudden jump is usually a new module. Read it when it moves.

### loom and mutation run on a schedule, not on every push

They are the two most expensive jobs per unit of work here. A gate that is slow
gets skipped, and a skipped gate is worse than no gate because it still shows
green.

---

## Running things

```sh
cargo test --workspace              # everything, debug
cargo t                             # alias, same thing
cargo tr                            # release; mandatory before any timing claim
cargo bench -p sniper-telemetry     # the always-on layer's cost
./tools/loom-check.sh               # every interleaving; needs RUSTFLAGS, the script sets it
cargo clippy --workspace --all-targets -- -D warnings
```

### Sandbox without the real program

```sh
cargo test -p sniper-sandbox                     # works on a clean checkout
cargo dump-pump                                  # fetch the real 10 MB dump
cargo test -p sniper-sandbox -- --ignored         # then run the fidelity test
```

The one ignored test asserts the real binary is ~10 MB. It cannot pass offline
by construction, and it is `#[ignore]`d rather than deleted so the capability
stays visible instead of quietly rotting.

---

## Conventions this suite follows

**Comments explain why, not what.** A comment restating the code is noise; a
comment explaining that a value is `u64` lamports rather than `f64` SOL, or that
the clamp is on the ratio rather than the numerator, saves the next reader an
investigation.

**Failures print the inputs.** A property test that fails without the case that
failed is a property test you will re-run instead of reading.

**Property tests draw from the domain, not from the type.** Generating a
`u64` supply spends most cases proving a range check works. Generating the two
measured standard supplies tests the thing that is actually interesting.

**Deterministic seeds.** No random seeds, no wall-clock dependence, no
`HashMap` iteration order in anything under test. A flaky test teaches people
to re-run failures instead of reading them.

**Tests that share global state are serial.** `leak_budget.rs` uses
`serial_test` because the interning set is process-wide, and a parallel run
would make each test's baseline depend on its neighbours' execution order.