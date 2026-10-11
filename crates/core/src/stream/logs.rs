//! 📡 Live log stream — the cheapest real-time feed that exists.
//!
//! ## Why this exists at all
//!
//! The obvious assumption for a sniper is Yellowstone gRPC. It is also a paid
//! feature on every major provider, and it was measured here before being
//! assumed either way.
//!
//! A plain WebSocket `logsSubscribe` turns out to carry enough to detect a
//! launch within about a block, on a free plan:
//!
//! * the program emits `Program log: Instruction: <Name>` in plain text, so the
//!   instruction is identified without parsing anything;
//! * `Program data:` carries the Anchor event, base64, complete — several
//!   hundred bytes, not truncated;
//! * the notification carries `err`, so failed transactions are recognisable
//!   before they can be mistaken for real activity.
//!
//! Measured against a free Helius key on mainnet, October 2026.
//!
//! ## What this feed deliberately does *not* do
//!
//! It does not tell us the mint. Logs have no account keys. The mint costs one
//! `getTransaction` call, which is fine for detection and reporting, and is the
//! reason a paid gRPC feed is still wanted for execution: on this path the
//! account list arrives a round trip later, and a round trip is the whole game.
//!
//! ## The trap this module exists to avoid
//!
//! Matching on log text looks like the cheap shortcut, and chapter 2 warned
//! that log matching produces false positives. That warning was about
//! *side effects* — every purchase mints an associated token account, so
//! searching for mint activity flags ordinary trading. Matching the program's
//! own `Instruction: <Name>` line is a different thing: the program states
//! which handler ran. The distinction is load-bearing, and
//! `only_the_pump_program_names_are_considered` guards it.

use serde::Deserialize;

/// The pump.fun program id, base58. Kept as a literal here so the stream module
/// has no dependency on the decoder and can be tested on its own.
pub const PUMP_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Prefix the runtime uses when a program logs its own instruction data.
const PROGRAM_DATA_PREFIX: &str = "Program data: ";

/// Prefix Anchor uses when a handler announces itself.
const INSTRUCTION_LOG: &str = "Program log: Instruction: ";

/// A transaction notification, as delivered by `logsSubscribe`.
///
/// The wire envelope nests the payload two levels deep — `params.result.value` —
/// and that shape is modelled rather than flattened, so a provider change shows
/// up as a parse error here instead of as silently empty fields.
#[derive(Debug, Clone, Deserialize)]
pub struct LogNotification {
    /// Notification body.
    pub params: NotificationParams,
}

/// Envelope between the JSON-RPC call and the transaction.
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationParams {
    /// Provider-assigned subscription id echoed back on every notification.
    #[serde(default)]
    pub subscription: u64,
    /// Slot context plus the transaction's logs.
    pub result: NotificationResult,
}

/// Envelope: `{"context": {...}, "value": {...}}`.
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationResult {
    /// Slot context.
    #[serde(default)]
    pub context: Context,
    /// The transaction's logs and outcome.
    pub value: TxLogs,
}

/// Slot the notification was produced in.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Context {
    /// Block number.
    #[serde(default)]
    pub slot: u64,
}

/// One transaction's worth of logs.
#[derive(Debug, Clone, Deserialize)]
pub struct TxLogs {
    /// Transaction signature, base58.
    #[serde(default)]
    pub signature: String,
    /// Failure reason, or `None` when the transaction succeeded.
    #[serde(default)]
    pub err: Option<serde_json::Value>,
    /// Program log lines, in order.
    #[serde(default)]
    pub logs: Vec<String>,
}

/// What a pump.fun transaction was doing, read off its own log lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpActivity {
    /// A token was launched. Carries the instruction name as emitted.
    Launch(&'static str),
    /// An ordinary trade.
    Trade(&'static str),
    /// Something the pump program did that is neither.
    Other(&'static str),
}

/// Instruction names, matched exactly as the program spells them.
///
/// Names come from the runtime's own log line, so they are facts about this
/// program rather than guesses. Anything not listed here is reported as
/// `Other` rather than guessed at: a wrong classification would be a bug, and
/// an unclassified one is merely information.
const KNOWN: &[(&str, PumpActivityKind)] = &[
    ("Create", PumpActivityKind::Launch),
    ("CreateV2", PumpActivityKind::Launch),
    ("Buy", PumpActivityKind::Trade),
    ("BuyV2", PumpActivityKind::Trade),
    ("BuyExactSolIn", PumpActivityKind::Trade),
    ("BuyExactQuoteIn", PumpActivityKind::Trade),
    ("Sell", PumpActivityKind::Trade),
    ("SellV2", PumpActivityKind::Trade),
];

/// Classification of an instruction name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpActivityKind {
    Launch,
    Trade,
    Other,
}

impl TxLogs {
    /// True when the transaction succeeded.
    ///
    /// Checked before anything else, because a failed transaction still
    /// produces every log line it reached. Reading `Instruction: Buy` out of a
    /// transaction that then reverted would count a trade that never happened —
    /// and the sample above shows failures arriving constantly, including on the
    /// pump program itself.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.err.is_none()
    }

    /// Log lines emitted while the outermost frame is the pump program.
    ///
    /// One walk, one rule, shared by all three readers below. `Program <id>
    /// invoke [n]` pushes; `Program <id> success` and `Program <id> failed`
    /// pop. A line counts when that stack is exactly `[pump]`.
    ///
    /// Two earlier attempts failed, and the reasons are worth keeping:
    ///
    /// * matching only `invoke [1]` never noticed a nested call, so the fee
    ///   program's `Instruction: GetFeesWithQuoteMint` was credited to pump —
    ///   the exact false positive chapter 2 warned about;
    /// * keying off the most recently seen `invoke` line never noticed a
    ///   *return*, so a pump event emitted after the nested calls was credited
    ///   to the callee instead.
    ///
    /// Depth numbers alone cannot fix either: they count CPI depth, so a second
    /// top-level instruction restarts at 1.
    fn pump_scoped_lines(&self) -> Vec<&str> {
        let mut scoped = Vec::new();
        let mut frames: Vec<&str> = Vec::new();
        for line in &self.logs {
            if let Some((program, _)) = invoked(line) {
                frames.push(program);
                continue;
            }
            if let Some((program, _)) = returned(line) {
                // Pop only on a match: a malformed line must not unwind frames
                // that never closed.
                if frames.last() == Some(&program) {
                    frames.pop();
                }
                continue;
            }
            if frames.len() == 1 && frames[0] == PUMP_PROGRAM_ID {
                scoped.push(line.as_str());
            }
        }
        scoped
    }

    /// True when any log line came from a top-level pump.fun handler.
    #[must_use]
    pub fn touched_pump_handler(&self) -> bool {
        !self.pump_scoped_lines().is_empty()
    }

    /// Instruction names the pump program announced at top level, in order.
    #[must_use]
    pub fn pump_instruction_names(&self) -> Vec<&str> {
        self.pump_scoped_lines()
            .into_iter()
            .filter_map(|line| line.strip_prefix(INSTRUCTION_LOG))
            .collect()
    }

    /// Anchor event payloads the pump program emitted at top level, base64.
    #[must_use]
    pub fn pump_event_payloads(&self) -> Vec<&str> {
        self.pump_scoped_lines()
            .into_iter()
            .filter_map(|line| line.strip_prefix(PROGRAM_DATA_PREFIX))
            .collect()
    }

    /// Classifies this transaction, if it is a pump.fun transaction at all.
    ///
    /// Returns the first launch found. A transaction carrying a launch and a
    /// buy is the dev-buy bundle, and reporting it twice would double-count the
    /// launch rate this is used to measure.
    #[must_use]
    pub fn pump_activity(&self) -> Option<PumpActivity> {
        let mut first_trade = None;
        let mut first_other = None;
        for name in self.pump_instruction_names() {
            let kind = match KNOWN.iter().find(|(k, _)| *k == name) {
                Some((_, PumpActivityKind::Launch)) => PumpActivityKind::Launch,
                Some((_, PumpActivityKind::Trade)) => PumpActivityKind::Trade,
                _ => PumpActivityKind::Other,
            };
            match kind {
                PumpActivityKind::Launch => {
                    return Some(PumpActivity::Launch(leak(name)));
                }
                PumpActivityKind::Trade if first_trade.is_none() => {
                    first_trade = Some(PumpActivity::Trade(leak(name)));
                }
                PumpActivityKind::Other if first_other.is_none() => {
                    first_other = Some(PumpActivity::Other(leak(name)));
                }
                _ => {}
            }
        }
        first_trade.or(first_other)
    }
}

/// Turns a log-derived `&str` into a `'static`, because the names only ever
/// come from a fixed table.
///
/// This is a measured trade: it requires that every name reaching it was looked
/// up in `KNOWN` or is an unknown name we want to surface verbatim. Unknown
/// names are turned into leaked strings on purpose — an unrecognised pump
/// instruction is the single most useful thing this stream can discover, and
/// interning it into a fixed table would throw that away.
fn leak(name: &str) -> &'static str {
    // Fast path: a known name is already `'static` in the table, so nothing is
    // allocated. This matters more than it looks -- `buy` and `sell` are the
    // overwhelming majority of traffic on this stream, and routing them through
    // the interning set would turn the rare case below into the common one.
    if let Some((known, _)) = KNOWN.iter().find(|(k, _)| *k == name) {
        return known;
    }
    intern_unknown(name)
}

/// Names interned so far, so an unrecognised instruction is kept alive for
/// reporting.
///
/// `OnceLock` because the set has to exist before the first unknown name and
/// has to be the *same* set for the lifetime of the process -- a local set
/// would dedupe within one call and still leak on the next.
static INTERNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
    std::sync::OnceLock::new();

/// Interns `name` exactly once and returns the same pointer thereafter.
///
/// # Panics
///
/// Panics if the interning mutex is poisoned, i.e. if another thread panicked
/// while holding it. That is a deliberate choice: the alternative is to drop
/// the name on the floor, which would make this module silently lose evidence
/// precisely when something has already gone wrong. A bot supervised by systemd
/// restarting is the better failure.
fn intern_unknown(name: &str) -> &'static str {
    let set = INTERNED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut guard = set.lock().expect("interning set mutex poisoned");

    if let Some(existing) = guard.get(name) {
        return existing;
    }

    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// How many distinct unrecognised names this process has interned.
///
/// Test-only accessor. Growth is bounded by the number of *distinct*
/// unrecognised instruction names rather than the number of times they occur,
/// and that is the claim `leak_budget.rs` verifies -- so the measurement has to
/// be observable, and a process-wide counter nobody can read cannot be
/// verified by anyone, including its author.
///
/// Returns a count rather than the set itself: handing out a `&'static
/// HashSet` would mean handing out a reference into a mutex guard's target
/// without the guard, which is the exact shape `unsafe_code = "forbid"` exists
/// to prevent. A `usize` cannot do that.
#[must_use]
pub fn interned_unknown_count() -> usize {
    INTERNED
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .map(|set| set.len())
        .unwrap_or_default()
}

/// Extracts `(program, depth)` from an `invoke` log line.
///
/// Depth is parsed and returned for diagnostics, but attribution deliberately
/// ignores it: this is CPI depth, so a second top-level instruction in the same
/// transaction logs `[1]` again.
fn invoked(line: &str) -> Option<(&str, u32)> {
    let rest = line.strip_prefix("Program ")?;
    let open = rest.rfind(" invoke [")?;
    let program = &rest[..open];
    let depth: u32 = rest[open + 9..].strip_suffix(']')?.parse().ok()?;
    Some((program, depth))
}

/// Extracts `(program, returned_cleanly)` from a frame-closing log line.
///
/// Both `Program <id> success` and `Program <id> failed: <reason>` end the
/// invocation, so both pop the stack; the flag records which happened. It is
/// carried rather than ignored because a reverts-then-continues sequence is
/// possible in a multi-instruction transaction, and a caller that cares can
/// tell the two apart.
///
/// `Program <id> consumed N of M compute units` also appears, and is
/// deliberately *not* matched: it is a progress report, not a return.
fn returned(line: &str) -> Option<(&str, bool)> {
    let rest = line.strip_prefix("Program ")?;
    if let Some(program) = rest.strip_suffix(" success") {
        return Some((program, true));
    }
    let (program, _) = rest.split_once(" failed: ")?;
    Some((program, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logs(lines: &[&str]) -> TxLogs {
        TxLogs {
            signature: "sig".to_string(),
            err: None,
            logs: lines.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn a_launch_is_recognised_from_the_programs_own_log_line() {
        let tx = logs(&[
            "Program ComputeBudget111111111111111111111111111111 invoke [1]",
            "Program ComputeBudget111111111111111111111111111111 success",
            &format!("Program {PUMP_PROGRAM_ID} invoke [1]"),
            "Program log: Instruction: CreateV2",
            &format!("Program {PUMP_PROGRAM_ID} success"),
        ]);
        assert_eq!(tx.pump_activity(), Some(PumpActivity::Launch("CreateV2")));
    }

    #[test]
    fn only_the_pump_program_names_are_considered() {
        // Two programs both log `Instruction: Sell`. Only the one inside the
        // pump boundary counts, and searching the raw log would attribute the
        // other program's trade to pump.fun.
        let tx = logs(&[
            "Program 11111111111111111111111111111111 invoke [1]",
            "Program log: Instruction: Sell",
            "Program 11111111111111111111111111111111 success",
            &format!("Program {PUMP_PROGRAM_ID} invoke [1]"),
            "Program log: Instruction: DistributeFeeToHolders",
            &format!("Program {PUMP_PROGRAM_ID} success"),
        ]);
        let names = tx.pump_instruction_names();
        assert_eq!(names, vec!["DistributeFeeToHolders"]);
        // Not a trade, not a launch: honestly reported as something else.
        assert_eq!(
            tx.pump_activity(),
            Some(PumpActivity::Other("DistributeFeeToHolders"))
        );
    }

    #[test]
    fn a_dev_buy_bundle_reports_exactly_one_launch() {
        // The shape measured on mainnet: `create_v2` immediately followed by a
        // buy, both top-level instructions. Both log `invoke [1]`, because the
        // bracket counts CPI depth and resets per top-level instruction — which
        // is why attribution cannot be built on the bracket alone.
        //
        // Counting this bundle twice would inflate the launch rate, the number
        // this stream exists to produce.
        let tx = logs(&[
            &format!("Program {PUMP_PROGRAM_ID} invoke [1]"),
            "Program log: Instruction: CreateV2",
            "Program data: Cgs=",
            &format!("Program {PUMP_PROGRAM_ID} success"),
            &format!("Program {PUMP_PROGRAM_ID} invoke [1]"),
            "Program log: Instruction: BuyExactQuoteIn",
            &format!("Program {PUMP_PROGRAM_ID} success"),
        ]);
        assert_eq!(
            tx.pump_instruction_names(),
            vec!["CreateV2", "BuyExactQuoteIn"]
        );
        assert_eq!(tx.pump_activity(), Some(PumpActivity::Launch("CreateV2")));
    }

    #[test]
    fn failed_transactions_are_recognisable() {
        // Measured on mainnet: pump instructions that revert still log their
        // handler name, and failures are frequent.
        let mut tx = logs(&[
            &format!("Program {PUMP_PROGRAM_ID} invoke [1]"),
            "Program log: Instruction: Sell",
            &format!("Program {PUMP_PROGRAM_ID} failed: custom program error: 0x8ae"),
        ]);
        assert!(tx.succeeded(), "constructed without an error field");
        tx.err = Some(serde_json::json!({ "InstructionError": [0, { "Custom": 2222 }] }));
        assert!(!tx.succeeded());
        // The name is still visible, which is exactly why `succeeded` is a
        // separate check rather than something callers can forget.
        assert_eq!(tx.pump_activity(), Some(PumpActivity::Trade("Sell")));
    }

    #[test]
    fn event_payloads_are_attributed_to_the_pump_program() {
        let tx = logs(&[
            "Program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb invoke [1]",
            "Program data: AQID",
            "Program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb success",
            &format!("Program {PUMP_PROGRAM_ID} invoke [1]"),
            "Program data: Cgs=",
            &format!("Program {PUMP_PROGRAM_ID} success"),
        ]);
        assert_eq!(tx.pump_event_payloads(), vec!["Cgs="]);
    }

    #[test]
    fn nested_invocation_depth_does_not_confuse_attribution() {
        // A pump instruction that calls another program mid-flight, then resumes
        // inside itself. Depth 2 belongs to the callee.
        let tx = logs(&[
            &format!("Program {PUMP_PROGRAM_ID} invoke [1]"),
            "Program log: Instruction: BuyExactSolIn",
            "Program pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ invoke [2]",
            "Program log: Instruction: GetFeesWithQuoteMint",
            "Program pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ success",
            "Program 11111111111111111111111111111111 invoke [2]",
            "Program log: Instruction: Transfer",
            "Program 11111111111111111111111111111111 success",
            "Program data: Cgs=",
            &format!("Program {PUMP_PROGRAM_ID} success"),
        ]);
        assert_eq!(tx.pump_instruction_names(), vec!["BuyExactSolIn"]);
        // The event after the nested calls is still the pump program's.
        assert_eq!(tx.pump_event_payloads(), vec!["Cgs="]);
    }

    #[test]
    fn a_notification_from_the_wire_parses() {
        // Exactly the shape seen on the free endpoint, trimmed.
        let raw = r#"{"jsonrpc":"2.0","method":"logsNotification","params":{"subscription":14342220,"result":{"context":{"slot":454023889},"value":{"signature":"2dmV","err":null,"logs":["Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]","Program log: Instruction: Sell","Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P success"]}}}}"#;
        let note: LogNotification = serde_json::from_str(raw).expect("wire shape must parse");
        assert_eq!(note.params.subscription, 14_342_220);
        assert_eq!(note.params.result.context.slot, 454_023_889);
        assert!(note.params.result.value.succeeded());
        assert_eq!(
            note.params.result.value.pump_activity(),
            Some(PumpActivity::Trade("Sell"))
        );
    }

    #[test]
    fn unknown_pump_instructions_are_surfaced_not_guessed() {
        let tx = logs(&[
            &format!("Program {PUMP_PROGRAM_ID} invoke [1]"),
            "Program log: Instruction: SomeBrandNewHandler",
        ]);
        assert_eq!(
            tx.pump_activity(),
            Some(PumpActivity::Other("SomeBrandNewHandler")),
            "an unrecognised handler must be reported, never renamed to something plausible"
        );
    }

    #[test]
    fn a_non_pump_transaction_yields_nothing() {
        let tx = logs(&[
            "Program ComputeBudget111111111111111111111111111111 invoke [1]",
            "Program ComputeBudget111111111111111111111111111111 success",
        ]);
        assert!(tx.pump_activity().is_none());
        assert_eq!(tx.pump_instruction_names(), Vec::<&str>::new());
    }
}
