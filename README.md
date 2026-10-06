# 🦅 sniper-core — Solana sniper bot

**The fastest free Solana sniper bot for pump.fun, PumpSwap and Raydium LaunchLab.**

```
┌──────────────────────────────────────────────────────────────────────────────┐
│  ARCHETYPE: high performance with minimal overhead and permanent            │
│  debug presence.                                                             │
│                                                                              │
│   🛡️ a leak of the private key   = losing every dollar   (main risk)         │
│   💸 a wasted transaction       = slow, silent money loss                   │
│   🐌 undetected latency         = a bot that simply never wins              │
│                                                                              │
│   Therefore: secrets are guarded by a scanner, every transaction is          │
│   simulated before it is signed, and every pipeline stage is measured.       │
└──────────────────────────────────────────────────────────────────────────────┘
```

---

## 📐 The archetype, concretely

Three rules, enforced by the code and by tests:

| Rule | Mechanism | Why |
|---|---|---|
| **Instrumentation is a build flag, not a runtime switch** | `sniper-telemetry` has a `debug-trace` feature. Without it, `Probe::start`/`finish` are `#[inline(always)]` empty functions that LLVM deletes. | A release binary must pay zero for observability. A runtime flag means someone can turn it on in production by accident. |
| **The always-on layer must be cheaper than a branch misprediction** | `Counter` / `Gauge` are `Relaxed` atomics (~1 ns, no locks, no allocation). | These stay on in every build because they are what an operator actually watches. |
| **Nothing in the hot path may allocate or block** | `TraceRing` has fixed capacity assigned once and evicts oldest. Draining happens on another thread. | An allocation in the hot path is a latency spike waiting for load. |

Both configurations are tested. `cargo t` proves probes cost nothing;
`cargo t --features sniper-telemetry/debug-trace` proves they record.

---

## 🗂 Layout

```
sniper-core/
├─ Cargo.toml              workspace, shared dep versions, release profile
├─ .cargo/config.toml      cargo aliases (t, tr, b, c, gate)
├─ .env.example            secret template — safe to commit
├─ .env                    REAL SECRETS — gitignored, verified by Gatekeeper
├─ crates/
│  ├─ telemetry/           measurement primitives · zero dependencies
│  ├─ core/                venues · config · pricing · risk · execution · positions
│  ├─ cli/                 the bot executable
│  └─ xtask/               Gatekeeper — the environment preflight
└─ fixtures/               program dumps, captured streams, price tracks
```

---

## 🎯 Venues

| Venue | Program | Trigger | Status |
|---|---|---|---|
| pump.fun bonding curve | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` | `create` / `create_v2` | ✅ id verified in `pump-fun/pump-public-docs` |
| PumpSwap (pAMM) | — | `CreatePoolEvent` | ⚠️ id pending IDL verification |
| Raydium LaunchLab | — | `initialize` / `initialize2` | ⚠️ id pending IDL verification |

The two unverified venues are **refused in live mode** by `Config::validate`.
A wrong program id means silently subscribing to nothing, which looks exactly
like "no tokens are launching" — so it is made impossible to run.

---

## 🔁 The chapter loop

Every chapter ships its own autotest system. A chapter is not done until its
system is green, and the next chapter does not start until then.

| # | Chapter | Autotest system | Metric |
|---|---|---|---|
| 0 | Environment and repository | 🛡️ `Gatekeeper` | exit 0 |
| 1 | LiteSVM stand + program dump | 🧪 `SandboxForge` | identical bytes |
| 2 | Launch-event decoder | 🧬 `DecodeMatrix` | 0 FP / 0 FN |
| 3 | Curve maths and quotes | 📐 `MathInvariants` | 0 violations / 50k |
| 4 | Transaction builder | 🥇 `TxGolden` | byte-stable snapshots |
| 5 | Execution fan-out | 🌐 `FanoutSim` | no infinite retries |
| 6 | Risk filters | 🥪 `RiskSandbox` | recall 100% |
| 7 | Live stream detector | 📼 `StreamReplay` | 0 losses on gaps |
| 8 | Positions and exits | 🎮 `PositionSim` | PnL reconciles |
| 9 | Latency budget | 🎯 `LatencyBudget` | < 2 ms hot path |
| 10 | Reliability and chaos | 🐒 `ChaosMonkey` | losses ≤ budget |
| 11 | Secrets and safety | 🔐 `SecretSentinel` | 0 keys in repo |
| 12 | Promotion gate | 🚦 `PromoGate` | stages in order |
| 13 | Linux migration | 🔀 `PortParity` | 100% parity |

Full plan with rationale: `C:\Users\user\.opencode\plan\solana-sniper-master-plan.md`

---

## ⚡ Commands

```powershell
cargo gate      # chapter 0 preflight — refuses to run on a broken environment
cargo t         # workspace tests, debug profile (fast loop)
cargo tr        # workspace tests, release profile (mandatory before benchmarking)
cargo b         # production build
cargo c         # clippy, warnings are errors
```

**Never measure latency in a debug build.** `dev` uses `opt-level = 1`, not 0,
but release is the only shape where the latency budget means anything.

---

## 💰 Operating point

| Setting | Value | Why |
|---|---|---|
| Position size | **0.02 SOL** | small enough that a bad run is survivable |
| Max open positions | 5 | exposure stays under 0.1 SOL |
| Max trades/day | 100 | bounds the daily tip + priority-fee burn |
| Max risk score | 3/10 | refuses the obviously hostile tokens |
| Default mode | `paper` | builds and subscribes for real, never broadcasts |

---

## ⚠️ Reality check

80-95% of new meme tokens are dead within minutes. Latency matters because it
buys a place in the top 1-3 by priority in the first slot — not because
"buying something new" is a strategy by itself. Chapter 12 exists so this is
discovered on paper, for the price of transaction fees, before it is
discovered with real money.

---

## 🔐 Secrets

* Real keys live in `.env`, which is gitignored.
* The GitHub token lives in the Windows **Credential Manager** via `gh`, never in a file.
* The deploy password lives in the Windows Credential Manager under `sniper-deploy`.
* `Gatekeeper` runs `git ls-files` plus a 64-char base58 scan on every invocation.
* `SecretSentinel` (chapter 11) extends this to the entire git history and
  installs a pre-commit hook. A seed committed once and deleted is still leaked.

---

## 🪟 → 🐧

Windows is the development and test platform: it builds, it runs LiteSVM, it
runs the whole test suite. Production moves to Linux (chapter 13) because of
epoll vs IOCP, core isolation, systemd supervision, and cost per unit of
latency. `PortParity` proves the move changed no decisions and no throughput
in the wrong direction.