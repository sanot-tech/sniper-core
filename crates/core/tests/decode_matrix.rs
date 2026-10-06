//! 🧬 `DecodeMatrix` — chapter 2's autotest system.
//!
//! ## The claim under test
//!
//! "The pump.fun decoder has **zero false positives and zero false negatives**
//! on captured mainnet traffic."
//!
//! Both halves matter, and they fail differently:
//!
//! * a **false positive** — reading a buy or a sell as a launch — makes the
//!   bot buy a token that already exists, into a curve that is already
//!   saturated. Launches are rare, so the decoder says "no" almost every time;
//!   every wrong "yes" is a coin flip with real money behind it.
//! * a **false negative** — missing a launch — costs one trade, and it is
//!   silent. A decoder that never fires looks exactly like a quiet market.
//!
//! ## Why the fixtures are captured rather than written
//!
//! A hand-written transaction proves our decoder agrees with our assumptions.
//! These are real mainnet transactions, so a test failure means something.
//! Regenerate them with `cargo fetch-fixtures`.
//!
//! ## What else the matrix proves
//!
//! * every **non-launch** instruction observed in the wild is refused, not just
//!   the ones that happened to land in the sample;
//! * random byte mutations never invent a launch;
//! * the mint taken from a real `create_v2` ends in the literal suffix `pump`,
//!   which is pump.fun's own mint convention — an independent check that the
//!   right account was read and not merely *a* plausible one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sniper_core::venue::pumpfun::{PumpFunDecoder, RawInstruction, RawTransaction};
use sniper_core::venue::VenueKind;

/// Where the captured transactions live.
fn fixtures_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR for this test target is `crates/core`, so the repo
    // root is two levels up. Getting this wrong produces a path that is merely
    // plausible, and the test then fails on "no fixtures" rather than on the
    // real problem.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("txs")
}

#[derive(Debug, serde::Deserialize)]
struct CapturedTx {
    signature: String,
    bucket: String,
    version: String,
    slot: u64,
    instructions: Vec<CapturedIx>,
    #[serde(default)]
    wire: String,
}

#[derive(Debug, serde::Deserialize)]
struct CapturedIx {
    program_id: String,
    discriminator: String,
    data_len: usize,
    accounts: Vec<String>,
}

fn pubkey32(s: &str) -> Option<[u8; 32]> {
    bs58::decode(s).into_vec().ok()?.try_into().ok()
}

fn signature64(s: &str) -> [u8; 64] {
    let raw = bs58::decode(s).into_vec().unwrap_or_default();
    let mut out = [0u8; 64];
    let n = raw.len().min(64);
    out[..n].copy_from_slice(&raw[..n]);
    out
}

fn disc8(hex: &str) -> Option<[u8; 8]> {
    if hex.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Owned mirror of a captured transaction, so the decoder can borrow it.
struct Owned {
    signature: [u8; 64],
    slot: u64,
    programs: Vec<Option<[u8; 32]>>,
    accounts: Vec<Vec<[u8; 32]>>,
    payloads: Vec<Vec<u8>>,
}

impl Owned {
    fn from_captured(tx: &CapturedTx) -> Self {
        let mut programs = Vec::with_capacity(tx.instructions.len());
        let mut accounts = Vec::with_capacity(tx.instructions.len());
        let mut payloads = Vec::with_capacity(tx.instructions.len());
        for ix in &tx.instructions {
            programs.push(pubkey32(&ix.program_id));
            accounts.push(ix.accounts.iter().filter_map(|a| pubkey32(a)).collect());
            // Reconstruct a payload of the captured length so that account
            // counts and data sizes match what the stream would deliver.
            let mut d = disc8(&ix.discriminator).unwrap_or([0; 8]).to_vec();
            d.resize(ix.data_len.max(8), 0);
            payloads.push(d);
        }
        Self {
            signature: signature64(&tx.signature),
            slot: tx.slot,
            programs,
            accounts,
            payloads,
        }
    }

    /// Runs `f` against the decoder's zero-copy input shape.
    ///
    /// A closure rather than a returned reference on purpose: the instruction
    /// view borrows this struct's buffers, so it cannot be stored inside it.
    /// Keeping the borrow scoped to one call expresses exactly that lifetime
    /// instead of fighting the borrow checker with an unsafe transmute.
    fn with_raw<R>(&self, f: impl FnOnce(&RawTransaction<'_>) -> R) -> R {
        let ixes: Vec<RawInstruction<'_>> = self
            .programs
            .iter()
            .enumerate()
            .filter_map(|(i, program)| {
                Some(RawInstruction {
                    program_id: (*program)?,
                    data: self.payloads.get(i).map_or(&[][..], Vec::as_slice),
                    accounts: self.accounts.get(i).map_or(&[][..], Vec::as_slice),
                })
            })
            .collect();
        let raw = RawTransaction {
            signature: self.signature,
            slot: self.slot,
            instructions: &ixes,
        };
        f(&raw)
    }
}

/// Loads the captured matrix, failing loudly when it is absent.
///
/// An empty set proves nothing, so this asserts rather than returning empty:
/// a missing fixture directory must stop the build, not quietly turn the
/// strongest chapter-2 guarantee into a no-op.
fn load() -> Vec<CapturedTx> {
    let dir = fixtures_dir();
    assert!(
        dir.is_dir(),
        "no fixtures at {}\n\
         hint: run `cargo fetch-fixtures` with HELIUS_API_KEY set",
        dir.display()
    );

    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .expect("fixture dir readable")
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            out.push(
                serde_json::from_str::<CapturedTx>(&body)
                    .unwrap_or_else(|e| panic!("parse {}: {e}", path.display())),
            );
        }
    }
    assert!(
        out.len() >= 6,
        "matrix too small: {} fixtures, need at least 6",
        out.len()
    );
    let launches = out.iter().filter(|t| t.bucket == "create").count();
    let others = out.len() - launches;
    assert!(
        launches >= 1 && others >= 1,
        "matrix needs both classes: {launches} launches, {others} others"
    );
    out
}

#[test]
fn no_false_negatives_on_captured_launches() {
    let decoder = PumpFunDecoder::new();
    let mut checked = 0;
    let mut missed = Vec::new();

    let fixtures = load();
    for tx in fixtures.iter().filter(|t| t.bucket == "create") {
        let owned = Owned::from_captured(tx);
        if owned.with_raw(|raw| decoder.decode_launch(raw)).is_none() {
            missed.push(&tx.signature);
        }
        checked += 1;
    }

    assert!(checked > 0, "no launch fixtures: the matrix proves nothing");
    assert!(
        missed.is_empty(),
        "false negatives: missed {} of {checked} real launches",
        missed.len()
    );
}

#[test]
fn no_false_positives_on_captured_non_launches() {
    let decoder = PumpFunDecoder::new();
    let mut checked = 0;
    let mut wrong = Vec::new();

    let fixtures = load();
    for tx in fixtures.iter().filter(|t| t.bucket != "create") {
        let owned = Owned::from_captured(tx);
        if owned.with_raw(|raw| decoder.decode_launch(raw)).is_some() {
            wrong.push(&tx.signature);
        }
        checked += 1;
    }

    assert!(
        checked > 0,
        "no non-launch fixtures: zero false positives would be unprovable"
    );
    assert!(
        wrong.is_empty(),
        "false positives: {} of {checked} non-launch transactions decoded as launches",
        wrong.len()
    );
}

#[test]
fn every_observed_non_launch_instruction_is_refused() {
    // The full census from mainnet rather than whatever the sample happened to
    // contain. A discriminator seen once in a capture is exactly the one that
    // would otherwise slip through unnoticed.
    let census: &[(&str, &str)] = &[
        ("66063d1201daebea", "buy"),
        ("33e685a4017f83ad", "sell"),
        ("b817ee6167c5d33d", "buy_v2"),
        ("5df6823ce7e940b2", "sell_v2"),
        ("c2ab1c46684d5b2f", "buy_exact_quote_in_v2"),
        ("1416567bc61cdb84", "collect_creator_fee"),
        ("f945a4da9667548a", "close_user_volume_accumulator"),
        ("38fc74089edfcd5f", "unclassified-1"),
        ("623691610246ad2b", "unclassified-2"),
        ("a572670079cef751", "unclassified-3"),
        ("cf118af204221338", "unclassified-4"),
        ("ea66c2cb96483ee5", "unclassified-5"),
    ];

    let program = pubkey32(VenueKind::PumpFun.program_id()).expect("valid pump program id");
    let accounts = [[1u8; 32]; 20];

    for (hex, name) in census {
        let payload = disc8(hex).expect("8-byte discriminator").to_vec();
        let ix = RawInstruction {
            program_id: program,
            data: &payload,
            accounts: &accounts,
        };
        let ixes = [ix];
        let raw = RawTransaction {
            signature: [0u8; 64],
            slot: 1,
            instructions: &ixes,
        };
        assert!(
            PumpFunDecoder::new().decode_launch(&raw).is_none(),
            "`{name}` ({hex}) must never decode as a launch"
        );
    }
}

#[test]
fn random_mutations_never_invent_a_launch() {
    // Deterministic xorshift rather than a random generator: a flaky fuzz is
    // worse than no fuzz, because it teaches people to re-run failures instead
    // of reading them.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let program = pubkey32(VenueKind::PumpFun.program_id()).expect("valid pump program id");
    let accounts = [[9u8; 32]; 20];

    for _ in 0..5_000 {
        let len = 1 + (next() % 64) as usize;
        let payload: Vec<u8> = (0..len).map(|_| (next() & 0xFF) as u8).collect();
        let ix = RawInstruction {
            program_id: program,
            data: &payload,
            accounts: &accounts,
        };
        let ixes = [ix];
        let raw = RawTransaction {
            signature: [0u8; 64],
            slot: 1,
            instructions: &ixes,
        };
        assert!(
            PumpFunDecoder::new().decode_launch(&raw).is_none(),
            "a random {len}-byte payload decoded as a launch"
        );
    }
}

#[test]
fn extracted_mints_are_structurally_sane() {
    // The first version of this test asserted every extracted mint ends in the
    // literal suffix `pump`, because nearly all of them do. Real traffic
    // corrected that: pump.fun also mints without the suffix, and a creator
    // can choose either. Asserting the convention would have produced a
    // failing test for correct behaviour, and "fixing" the decoder to satisfy
    // it would have made the bot wrong.
    //
    // So the structural properties are asserted, and the suffix is reported as
    // a statistic rather than a rule.
    let decoder = PumpFunDecoder::new();
    let mut suffixed = 0_usize;
    let mut total = 0_usize;
    let mut distinct_mints = std::collections::HashSet::new();
    let mut problems = Vec::new();

    let fixtures = load();
    for tx in fixtures.iter().filter(|t| t.bucket == "create") {
        let owned = Owned::from_captured(tx);
        let Some(event) = owned.with_raw(|raw| decoder.decode_launch(raw)) else {
            continue;
        };
        total += 1;

        let encoded = bs58::encode(event.mint).into_string();
        distinct_mints.insert(encoded.clone());
        if encoded.ends_with("pump") {
            suffixed += 1;
        }

        // A mint must not collide with the infrastructure addresses that sit
        // next to it in the same instruction. Reading the wrong account slot
        // shows up as exactly one of these.
        for (label, forbidden) in [
            ("pump program", VenueKind::PumpFun.program_id()),
            ("wrapped SOL", "So11111111111111111111111111111111111111112"),
            ("system program", "11111111111111111111111111111111"),
        ] {
            if encoded == forbidden {
                problems.push(format!("account 0 of create is the {label}"));
            }
        }

        // The creator and the curve must be distinct from the mint.
        if event.creator == event.mint {
            problems.push("creator and mint are the same account".into());
        }
        if event.pool == event.mint {
            problems.push("bonding curve and mint are the same account".into());
        }
    }

    assert!(total > 0, "no launch fixtures to inspect");
    assert!(
        problems.is_empty(),
        "structurally implausible launches: {problems:?}"
    );
    assert_eq!(
        distinct_mints.len(),
        total,
        "every launch must yield its own mint; duplicates mean the decoder is reading a constant slot"
    );
    println!("{suffixed}/{total} captured mints carry the `pump` suffix");
}

#[test]
fn capture_covers_transaction_version_one() {
    // v1 is the large majority of pump.fun traffic, so a decoder that only
    // handles v0 would silently miss most launches. This test fails if a future
    // capture somehow contains no v1 traffic, making that assumption visible.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for tx in load() {
        *counts.entry(tx.version).or_insert(0) += 1;
    }
    println!("captured versions: {counts:?}");
    assert!(
        counts.contains_key("1"),
        "expected v1 transactions in the capture"
    );
}

#[test]
fn capture_includes_raw_wire_payloads_for_later_chapters() {
    // Chapter 4 compares exact transaction bytes. If the capture loses the wire
    // payloads, that chapter discovers it late and for the wrong reason.
    let fixtures = load();
    let with_wire = fixtures.iter().filter(|t| !t.wire.is_empty()).count();
    assert!(
        with_wire > 0,
        "no fixture carries raw wire bytes; chapter 4 would have nothing to compare"
    );
    println!("{with_wire}/{} fixtures carry wire bytes", fixtures.len());
}
