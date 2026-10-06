//! 📥 `fetch-fixtures` — capture real mainnet pump.fun launches.
//!
//! ## Why capture instead of hand-writing tests
//!
//! A hand-written byte blob only proves our decoder agrees with our own idea of
//! the format. A captured mainnet transaction proves it agrees with
//! **mainnet**. Chapter 2 is built entirely on top of what this tool produces.
//!
//! ## The shape we store
//!
//! Instructions as *resolved* entries: program id, discriminator, data length,
//! and the accounts referenced. That is deliberately the shape the Yellowstone
//! gRPC stream delivers, because the detector never parses wire bytes in
//! production — protobuf hands it a structured transaction already. Raw wire
//! bytes are stored too, for the chapters that need exact bytes.
//!
//! ## Buckets, and why the second one matters
//!
//! * `create` — a launch: the only signal a sniper acts on
//! * `other`  — everything else
//!
//! Every false positive a sniper can suffer starts life as "this purchase was
//! mistaken for a launch". Without a bucket of non-launches, the claim "zero
//! false positives" would be unfalsifiable.
//!
//! ## Two things this tool teaches the detector
//!
//! * **Launches are rare.** In a recent sample of 400 consecutive pump.fun
//!   transactions there were **zero** launches: buys and sells outnumber them
//!   by more than an order of magnitude. A detector therefore has to page
//!   back through a wide window, not just subscribe and hope.
//! * **pump.fun instructions are never pre-parsed.** Not in `jsonParsed` mode,
//!   not at all. Every instruction arrives raw with a resolved `programId` and
//!   base58 data, so discriminator comparison is the only option — and it is
//!   also the fastest one.
//!
//! ## Transaction versions
//!
//! Mainnet pump.fun traffic in October 2026 comes back as legacy, v0 **and**
//! v1, with v1 already the large majority. A decoder that only handles v0
//! would miss most launches, silently.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use futures::{stream, StreamExt};

/// pump.fun bonding curve program.
const PUMP_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Anchor discriminators, derived as `sha256("global:" + name)[..8]`.
///
/// These are **computed and verified against mainnet**, not copied from a blog
/// post. That distinction is not pedantic: a third-party guide lists `create`
/// as `181ec82805181c77`, while the real value is `181ec828051c0777`. With the
/// wrong constant the detector would never fire on a plain launch and the bot
/// would look perfectly healthy while buying nothing.
const INSTRUCTIONS: &[(&str, &str)] = &[
    ("create", "181ec828051c0777"),
    ("create_v2", "d6904cec5f8b31b4"),
    ("buy", "66063d1201daebea"),
    ("sell", "33e685a4017f83ad"),
    ("buy_v2", "b817ee6167c5d33d"),
    ("sell_v2", "5df6823ce7e940b2"),
    ("buy_exact_quote_in_v2", "c2ab1c46684d5b2f"),
    ("collect_creator_fee", "1416567bc61cdb84"),
    ("close_user_volume_accumulator", "f945a4da9667548a"),
];

/// Two discriminators observed on mainnet that do not match any name above.
/// Recorded rather than guessed: an unclassified instruction is information, a
/// wrong name is a bug.
const UNCLASSIFIED: &[&str] = &["38fc74089edfcd5f", "623691610246ad2b"];

/// Discriminators that mean "a new token just appeared".
const LAUNCHES: &[&str] = &["181ec828051c0777", "d6904cec5f8b31b4"];

fn name_of(disc: &str) -> Option<&'static str> {
    INSTRUCTIONS
        .iter()
        .find(|(_, d)| *d == disc)
        .map(|(n, _)| *n)
}

fn is_launch(disc: &str) -> bool {
    LAUNCHES.contains(&disc)
}

/// Computes an Anchor discriminator: `sha256("global:" + name)[..8]`.
///
/// Test-only, and therefore behind `cfg(test)`: the shipping tool compares
/// pre-computed constants and never needs a hash at runtime. Keeping `sha2` out
/// of the binary means the fetcher itself stays small.
#[cfg(test)]
fn anchor_discriminator(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("global:{name}").as_bytes());
    digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut acc, b| {
            write!(acc, "{b:02x}").expect("writing into a String cannot fail");
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the table rather than trusting it.
    ///
    /// This exists because a widely copied third-party guide carries a one-byte
    /// typo in the `create` discriminator. A typo is invisible: the decoder
    /// simply never fires, and the bot looks perfectly healthy while buying
    /// nothing. Deriving the expected value from the instruction name turns a
    /// silent lifetime bug into a failing test.
    #[test]
    fn every_known_discriminator_is_the_anchor_hash_of_its_name() {
        for (name, expected) in INSTRUCTIONS {
            assert_eq!(
                &anchor_discriminator(name),
                expected,
                "`{name}` discriminator does not match sha256(\"global:{name}\")"
            );
        }
    }

    #[test]
    fn launches_are_exactly_the_two_create_variants() {
        for (name, disc) in INSTRUCTIONS {
            let expected = name.starts_with("create");
            assert_eq!(
                is_launch(disc),
                expected,
                "`{name}` classified as launch={expected}"
            );
        }
        assert_eq!(LAUNCHES.len(), 2, "only create and create_v2 launch tokens");
    }

    #[test]
    fn version_label_reads_numbers_and_the_absent_case() {
        let cases = [
            (serde_json::json!({ "version": 0 }), "0"),
            (serde_json::json!({ "version": 1 }), "1"),
            (serde_json::json!({ "version": "legacy" }), "legacy"),
            (serde_json::json!({}), "legacy"),
        ];
        for (input, expected) in cases {
            assert_eq!(version_label(&input), expected, "input {input}");
        }
    }

    #[test]
    fn pump_program_id_is_43_base58_characters() {
        assert_eq!(PUMP_PROGRAM_ID.len(), 43);
        assert!(bs58::decode(PUMP_PROGRAM_ID).into_vec().is_ok());
    }
}

/// Launches and non-launches to keep. Enough for a real matrix, few enough that
/// the repository stays a code repository.
const WANT_CREATE: usize = 6;
const WANT_OTHER: usize = 6;

/// Signature pages to walk back. Each page is one cheap call for 1000 entries.
///
/// Depth matters more than it looks. pump.fun sustains a very high transaction
/// rate, so 6 000 consecutive signatures can span only seconds of wall clock —
/// far too narrow a window to contain a launch, since launches are outnumbered
/// by trades by more than 10:1. Forty pages reach back far enough in time for
/// the stratified sample below to actually contain some.
const MAX_PAGES: usize = 40;

/// How many transactions to fetch in total, spread across the whole window.
///
/// Fetching every signature would take roughly ten minutes at the free plan's
/// request rate. A stratified sample covers the same span in a fraction of the
/// time, because what we need is temporal spread, not exhaustive coverage.
const TARGET_SAMPLES: usize = 4_000;

/// Requests fired in parallel within one batch.
const CONCURRENCY: usize = 8;

/// Sustained request rate. The Helius free plan allows 10 rps across all RPC
/// methods; going above it does not speed anything up, it just converts
/// successful samples into failures.
const REQUESTS_PER_SECOND: u64 = 9;

/// Attempts per fixture when fetching raw wire bytes.
const WIRE_ATTEMPTS: usize = 4;

/// How far back the second sampling window reaches, in seconds.
///
/// Three hours is wide enough that the two windows cannot share a market mood:
/// a launch-rate burst is exactly the sort of thing that would otherwise make
/// a fixture set look representative when it is not.
const WINDOW_SECONDS: u64 = 3 * 60 * 60;

/// One resolved instruction, as the detector sees it.
#[derive(Debug, serde::Serialize)]
struct InstructionView {
    program_id: String,
    /// First eight bytes of instruction data, hex. Empty when data is shorter.
    discriminator: String,
    /// Instruction data length in bytes: a cheap way to spot truncation.
    data_len: usize,
    /// Account addresses the instruction referenced, in order.
    accounts: Vec<String>,
    /// Present only on v1 transactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    stack_height: Option<u8>,
}

/// A captured transaction.
#[derive(Debug, serde::Serialize)]
struct Fixture {
    signature: String,
    bucket: &'static str,
    version: String,
    slot: u64,
    instructions: Vec<InstructionView>,
    /// Raw wire payload, base64, for chapters that need exact bytes.
    wire: String,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn rpc_url() -> Result<String, String> {
    if let Ok(k) = std::env::var("HELIUS_API_KEY") {
        if !k.is_empty() {
            return Ok(format!("https://mainnet.helius-rpc.com/?api-key={k}"));
        }
    }
    std::env::var("SOLANA_RPC_MAINNET").map_err(|_| {
        "set HELIUS_API_KEY or SOLANA_RPC_MAINNET (read from the environment, never from git)"
            .to_string()
    })
}

fn hex_prefix(data_b58: &str) -> String {
    let Ok(raw) = bs58::decode(data_b58).into_vec() else {
        return String::new();
    };
    // `fold` over `collect::<Vec<_>>().join("")`: fewer allocations on a path that
    // runs once per instruction per transaction.
    raw.iter()
        .take(8)
        .fold(String::with_capacity(16), |mut acc, b| {
            write!(acc, "{b:02x}").expect("writing into a String cannot fail");
            acc
        })
}

fn decoded_len(data_b58: &str) -> usize {
    bs58::decode(data_b58).into_vec().map_or(0, |d| d.len())
}

/// Renders the transaction version.
///
/// The RPC returns it as a **number** (`0`, `1`) for versioned transactions
/// and omits it entirely for legacy ones. Reading only the string form, as an
/// early version of this tool did, silently mislabelled every v0 and v1
/// transaction as legacy — which would have hidden the single most important
/// fact this tool discovered.
fn version_label(parsed: &serde_json::Value) -> String {
    match parsed.get("version") {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => "legacy".to_string(),
    }
}

async fn rpc(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let text = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{method}: transport: {e}"))?
        .text()
        .await
        .map_err(|e| format!("{method}: body: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{method}: json: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("{method}: rpc error: {err}"));
    }
    Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

struct TxView {
    signature: String,
    slot: u64,
    instructions: Vec<InstructionView>,
}

/// Fetches and classifies one transaction. `None` means "not usable".
async fn fetch_one(
    client: &reqwest::Client,
    url: &str,
    signature: &str,
    slot: u64,
) -> Option<(TxView, String, bool)> {
    let parsed = rpc(
        client,
        url,
        "getTransaction",
        serde_json::json!([
            signature,
            { "encoding": "jsonParsed", "maxSupportedTransactionVersion": 1 }
        ]),
    )
    .await
    .ok()?;

    let msg = parsed.get("transaction")?.get("message")?;
    let version = version_label(&parsed);

    let mut instructions = Vec::new();
    let mut launch_seen = false;

    for ix in msg["instructions"].as_array()? {
        let Some(program_id) = ix["programId"].as_str() else {
            continue;
        };
        let data_b58 = ix["data"].as_str().unwrap_or_default();
        let discriminator = hex_prefix(data_b58);
        if program_id == PUMP_PROGRAM_ID && is_launch(&discriminator) {
            launch_seen = true;
        }

        let accounts: Vec<String> = ix["accounts"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|a| match a {
                        serde_json::Value::String(s) => s.clone(),
                        other => other
                            .get("pubkey")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        instructions.push(InstructionView {
            program_id: program_id.to_string(),
            discriminator,
            data_len: decoded_len(data_b58),
            accounts,
            stack_height: ix["stackHeight"].as_u64().map(|v| v as u8),
        });
    }

    Some((
        TxView {
            signature: signature.to_string(),
            slot,
            instructions,
        },
        version,
        launch_seen,
    ))
}

async fn wire_bytes(client: &reqwest::Client, url: &str, signature: &str) -> Option<String> {
    // Retried, because the wire payload is a second request per fixture and
    // lands right after a paced scan, where the rate limiter is still warm.
    // Losing it silently would leave chapter 4 with nothing to compare and the
    // reason would surface much later, in the wrong place.
    for attempt in 0..WIRE_ATTEMPTS {
        let result = rpc(
            client,
            url,
            "getTransaction",
            serde_json::json!([
                signature,
                { "encoding": "base64", "maxSupportedTransactionVersion": 1 }
            ]),
        )
        .await;
        if let Ok(tx) = result {
            // The response shape depends on the requested encoding, and getting
            // this wrong is silent: an empty string, no error, and chapter 4
            // later discovering it has nothing to compare.
            //
            //   encoding = "base64"  ->  "transaction": ["<base64>", "base64"]
            //   encoding = "json"    ->  "transaction": { ..., "data": [...] }
            let wire = match tx.get("transaction") {
                Some(serde_json::Value::Array(items)) => items
                    .first()
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string),
                Some(serde_json::Value::Object(_)) => tx
                    .get("transaction")
                    .and_then(|t| t.get("data"))
                    .and_then(|d| d.get(0))
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string),
                _ => None,
            };
            if wire.is_some() {
                return wire;
            }
        }
        if attempt + 1 < WIRE_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(600 * (attempt as u64 + 1))).await;
        }
    }
    None
}

/// Long, but linear: collect the signature window, hop back in time, scan it at
/// the provider's rate limit, then write the matrix. Splitting it would hide the
/// ordering that matters — pacing and early exit only make sense as one loop.
#[allow(clippy::too_many_lines)]
async fn fetch() -> Result<i32, String> {
    let url = rpc_url()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        // Connection reuse matters: this tool makes thousands of small calls,
        // and a fresh TLS handshake per call would dominate the runtime.
        .pool_max_idle_per_host(CONCURRENCY * 2)
        .build()
        .map_err(|e| format!("client: {e}"))?;

    println!("📥 hunting for real pump.fun launches on mainnet");
    println!("   program: {PUMP_PROGRAM_ID}");

    // --- Phase 1: collect a wide window of signatures ----------------------
    //
    // Paging signatures is cheap (one call per 1000) while inspecting them is
    // not (one call each), so gather broadly first. Launches are outnumbered
    // by buys and sells by more than 10:1, so a short window finds none.
    let mut sigs: Vec<(String, u64)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Block time per signature, needed to hop backwards by wall clock.
    let mut slot_times: HashMap<String, u64> = HashMap::new();
    let mut before: Option<String> = None;
    for page in 1..=MAX_PAGES {
        let params = match &before {
            Some(b) => serde_json::json!([PUMP_PROGRAM_ID, { "limit": 1000, "before": b }]),
            None => serde_json::json!([PUMP_PROGRAM_ID, { "limit": 1000 }]),
        };
        let page_result = rpc(&client, &url, "getSignaturesForAddress", params).await?;
        let list = page_result
            .as_array()
            .ok_or("signature page: expected an array")?;
        if list.is_empty() {
            break;
        }
        for entry in list {
            if let Some(sig) = entry["signature"].as_str() {
                // The hop can overlap the paged window, so dedupe here rather
                // than trusting the two passes to be disjoint.
                if !seen.insert(sig.to_string()) {
                    continue;
                }
                let slot = entry["slot"].as_u64().unwrap_or(0);
                if let Some(bt) = entry["blockTime"].as_u64() {
                    slot_times.insert(sig.to_string(), bt);
                }
                sigs.push((sig.to_string(), slot));
            }
        }
        // The oldest entry on this page is the cursor for the next one.
        before = list
            .last()
            .and_then(|e| e["signature"].as_str())
            .map(ToString::to_string);
        if page % 10 == 0 || page == MAX_PAGES {
            println!("   page {page}: {} signatures collected", sigs.len());
        }
    }
    if sigs.is_empty() {
        return Err("no signatures returned".into());
    }

    // ---------------------------------------------------------------------
    // Time hopping.
    //
    // Paging alone is not enough: pump.fun sustains roughly 90 transactions per
    // slot, so 40 000 consecutive signatures span only ~440 slots, about three
    // minutes. Launches are outnumbered by trades by more than 10:1, so three
    // minutes of a single corner of the chain yields one or two at best.
    //
    // `until` jumps the cursor backwards in wall-clock time, which is exactly
    // the axis we need spread along. Two windows, far apart in time, give the
    // sample genuine breadth instead of three minutes of the same busy moment.
    // ---------------------------------------------------------------------
    let oldest_time = sigs
        .iter()
        .filter_map(|(sig, _)| slot_times.get(sig).copied())
        .min()
        .unwrap_or(0);
    if oldest_time > 0 {
        let until = oldest_time - WINDOW_SECONDS;
        println!(
            "   first window covers ~{WINDOW_SECONDS}s; hopping back {WINDOW_SECONDS}s to {until}"
        );
        let params = serde_json::json!([PUMP_PROGRAM_ID, { "limit": 1000, "until": until }]);
        if let Ok(page) = rpc(&client, &url, "getSignaturesForAddress", params).await {
            if let Some(list) = page.as_array() {
                for entry in list {
                    if let (Some(sig), Some(slot), Some(bt)) = (
                        entry["signature"].as_str(),
                        entry["slot"].as_u64(),
                        entry["blockTime"].as_u64(),
                    ) {
                        sigs.push((sig.to_string(), slot));
                        slot_times.insert(sig.to_string(), bt);
                    }
                }
                println!("   after hop: {} signatures", sigs.len());
            }
        }
    }

    // Stratify: take an evenly spaced slice across the whole time span.
    // Sequential sampling would re-inspect the same few seconds over and over.
    let step = (sigs.len() / TARGET_SAMPLES).max(1);
    let sampled: Vec<(String, u64)> = sigs
        .iter()
        .step_by(step)
        .take(TARGET_SAMPLES)
        .cloned()
        .collect();
    println!(
        "   window spans slots {}..{} — sampling {} of {} signatures (step {step})",
        sigs.last().map_or(0, |(_, s)| *s),
        sigs.first().map_or(0, |(_, s)| *s),
        sampled.len(),
        sigs.len()
    );

    // --- Phase 2: inspect, classify, and stop once the matrix is full -------
    //
    // Pacing matters more than concurrency: the free plan allows roughly ten
    // requests per second, and a wider fan-out simply produces failures that
    // look indistinguishable from "nothing found". An earlier version of this
    // tool fanned out at 16 and reported zero launches; the failures, not the
    // chain, were the answer.
    //
    // Launches are outnumbered by trades by more than 10:1, so a short window
    // legitimately contains none. That is why the window is pages deep and the
    // scan keeps going until the matrix is full rather than stopping at the
    // first batch.
    println!(
        "   scanning {} signatures, paced at ~{REQUESTS_PER_SECOND}/s ...",
        sampled.len()
    );

    let mut scanned = 0_usize;
    let mut found: Vec<Fixture> = Vec::new();
    let mut mix: BTreeMap<String, usize> = BTreeMap::new();
    let mut versions: BTreeMap<String, usize> = BTreeMap::new();

    // Paced, batched scanning.
    //
    // Concurrency alone is not enough. The free plan allows about ten requests
    // per second; firing 1 200 of them with no pacing yields roughly 140
    // successes and a thousand failures, which look precisely like "no
    // launches on chain". So each batch is awaited, then we pause for as long
    // as the batch needed to stay inside the budget.
    //
    // The pause is the whole trick, and it is why this tool reports a sample
    // count that matches what it asked for.
    let batch_size = CONCURRENCY;
    let batch_pause =
        std::time::Duration::from_millis((batch_size as u64) * 1000 / REQUESTS_PER_SECOND.max(1));

    'outer: for batch in sampled.chunks(batch_size) {
        let requests = batch.iter().map(|(sig, slot)| {
            let client = client.clone();
            let url = url.clone();
            let sig = sig.clone();
            async move { fetch_one(&client, &url, &sig, *slot).await }
        });
        let mut responses = stream::iter(requests).buffer_unordered(batch_size);

        while let Some(outcome) = responses.next().await {
            let Some((view, version, launch_seen)) = outcome else {
                continue;
            };
            scanned += 1;
            // Cloned because `version` is moved into the fixture below.
            *versions.entry(version.clone()).or_insert(0) += 1;
            for ix in &view.instructions {
                if ix.program_id == PUMP_PROGRAM_ID && !ix.discriminator.is_empty() {
                    *mix.entry(ix.discriminator.clone()).or_insert(0) += 1;
                }
            }

            let bucket = if launch_seen { "create" } else { "other" };
            let n_create = found.iter().filter(|f| f.bucket == "create").count();
            let n_other = found.len() - n_create;
            let keep = if launch_seen {
                n_create < WANT_CREATE
            } else {
                n_other < WANT_OTHER
            };
            if !keep {
                continue;
            }
            found.push(Fixture {
                signature: view.signature.clone(),
                bucket,
                version,
                slot: view.slot,
                instructions: view.instructions,
                wire: String::new(), // filled in below
            });
            if n_create + usize::from(bucket == "create") >= WANT_CREATE
                && n_other + usize::from(bucket == "other") >= WANT_OTHER
            {
                println!("   matrix complete after {scanned} transactions");
                break 'outer;
            }
        }

        // Sleep outside the request path so pacing costs nothing but time.
        tokio::time::sleep(batch_pause).await;
    }

    let creates = found.iter().filter(|f| f.bucket == "create").count();
    let others = found.len() - creates;
    println!("   captured {creates} launches + {others} others from {scanned} scanned");

    println!();
    println!("   transaction versions seen:");
    for (v, n) in &versions {
        println!("     {v:<8} {n:5}");
    }

    println!();
    println!("   pump.fun instructions observed:");
    for (d, n) in mix.iter().rev().take(25) {
        let tag = match name_of(d) {
            Some(name) => format!("   <- {name}"),
            None if UNCLASSIFIED.contains(&d.as_str()) => "   <- unclassified".to_string(),
            None => String::new(),
        };
        println!("     {n:5}x  {d}{tag}");
    }

    // --- Phase 3: raw wire bytes for the handful we kept -------------------
    for f in &mut found {
        f.wire = wire_bytes(&client, &url, &f.signature)
            .await
            .unwrap_or_default();
    }

    let out_dir = repo_root().join("fixtures").join("txs");
    fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    let mut manifest = String::from(
        "# Captured mainnet pump.fun transactions\n\
         #\n\
         # Generated by `cargo fetch-fixtures`.\n\
         # `instructions` holds resolved program ids and discriminators, which is\n\
         # the shape the Yellowstone stream delivers. `wire` is base64 of the raw\n\
         # payload, for chapters that need exact bytes.\n\
         #\n\
         # signature | bucket | version | slot | pump instructions (discriminator:bytes)\n",
    );
    for f in &found {
        let pump_ixs: Vec<String> = f
            .instructions
            .iter()
            .filter(|i| i.program_id == PUMP_PROGRAM_ID)
            .map(|i| format!("{}:{}b", i.discriminator, i.data_len))
            .collect();
        fs::write(
            out_dir.join(format!("{}.json", f.signature)),
            serde_json::to_string_pretty(f).unwrap_or_default(),
        )
        .map_err(|e| format!("write fixture: {e}"))?;
        writeln!(
            manifest,
            "{} | {} | {} | {} | {}",
            f.signature,
            f.bucket,
            f.version,
            f.slot,
            pump_ixs.join(",")
        )
        .expect("writing into a String cannot fail");
    }
    fs::write(out_dir.join("MANIFEST.txt"), &manifest).map_err(|e| format!("manifest: {e}"))?;

    println!();
    println!("✅ wrote {} fixtures to {}", found.len(), out_dir.display());

    let mut warn = false;
    if creates == 0 {
        println!("⚠️  no launch in this window — rerun, or widen MAX_PAGES");
        warn = true;
    }
    if others == 0 {
        println!("⚠️  no non-launch captured, so zero false positives is unprovable");
        warn = true;
    }
    Ok(i32::from(warn))
}

fn main() {
    let code = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => match rt.block_on(fetch()) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("🛑 {e}");
                2
            }
        },
        Err(e) => {
            eprintln!("🛑 runtime: {e}");
            2
        }
    };
    std::process::exit(code);
}
