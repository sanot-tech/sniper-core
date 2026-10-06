//! 🎯 `watch-launches` — live pump.fun launch detector.
//!
//! Connects to a Solana WebSocket, subscribes to pump.fun logs, and prints
//! every launch as it happens on mainnet. Nothing is simulated and nothing is
//! replayed: this is the real chain.
//!
//! ```text
//!   cargo watch              # 90 seconds
//!   cargo watch -- --seconds 600
//! ```
//!
//! ## What it proves and what it does not
//!
//! It proves detection: the decoder fires on real launches, at the speed a
//! WebSocket allows, on a free plan. It does **not** trade. Nothing here
//! signs, sends or spends. The number that matters for profit — whether a
//! 0.02 SOL entry at this latency still clears the bonding curve — needs
//! execution, which is chapter 5.
//!
//! ## Why the mint costs a round trip
//!
//! Logs carry no account keys. A launch is identified from the program's own
//! `Instruction: Create*` line, and the mint is then read from the transaction.
//! On a paid gRPC feed both arrive together; here they do not, and the delay is
//! exactly the kind of thing chapter 9's latency budget has to account for.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use sniper_core::stream::logs::{LogNotification, PumpActivity};
use sniper_core::venue::pumpfun::{PumpFunDecoder, RawInstruction, RawTransaction};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// How often to print a progress line. Frequent enough to show life, rare
/// enough not to bury the launches, which are the point.
const STATUS_EVERY: Duration = Duration::from_secs(30);

/// Entry point.
///
/// Long, because it is a straight pipeline — subscribe, read, classify, print,
/// report — and splitting it would hide the ordering that makes the latency
/// numbers meaningful.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = parse_seconds();
    let key = std::env::var("HELIUS_API_KEY").map_err(|_| {
        "| 🛑 set HELIUS_API_KEY (the free plan is enough; read from the environment, never from git)"
    })?;
    let ws_url = format!("wss://mainnet.helius-rpc.com/?api-key={key}");
    let http_url = format!("https://mainnet.helius-rpc.com/?api-key={key}");

    println!("🎯 pump.fun live launch detector");
    println!("   watching for {seconds}s — nothing is signed, nothing is spent\n");

    let (mut socket, _) =
        tokio_tungstenite::connect_async(ws_url.clone().into_client_request()?).await?;

    // `params` is positional: [filter, RpcTransactionLogsConfig]. A dangling
    // `"commitment"` key inside that array is answered with "Parse error",
    // which says nothing useful about what was wrong.
    let subscribe = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"logsSubscribe","params":[{{"mentions":["{}"]}},{{"commitment":"confirmed"}}]}}"#,
        sniper_core::PUMP_PROGRAM_ID
    );
    socket.send(Message::Text(subscribe)).await?;

    let decoder = PumpFunDecoder::new();
    let client = reqwest::Client::new();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);

    let mut launches: Vec<Launch> = Vec::new();
    let mut trades = 0_u64;
    let mut others = 0_u64;
    let mut failed = 0_u64;
    let mut malformed = 0_u64;
    let mut names: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut last_status = Instant::now();

    loop {
        if Instant::now() >= deadline {
            break;
        }
        // A short read timeout keeps the deadline honest without polling a
        // separate timer.
        let next = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
        let Ok(Some(Ok(message))) = next else {
            continue;
        };
        let Message::Text(text) = message else {
            continue;
        };

        let Ok(note) = serde_json::from_str::<LogNotification>(&text) else {
            // The subscription acknowledgement is not a notification; anything
            // else that fails to parse is counted, not ignored.
            if text.contains("\"id\":1") {
                println!("   ✅ subscribed");
            } else {
                malformed += 1;
            }
            continue;
        };

        let tx = &note.params.result.value;
        let slot = note.params.result.context.slot;

        if !tx.succeeded() {
            failed += 1;
            continue;
        }
        let Some(activity) = tx.pump_activity() else {
            continue;
        };
        for name in tx.pump_instruction_names() {
            *names.entry(name.to_string()).or_insert(0) += 1;
        }

        match activity {
            PumpActivity::Launch(name) => {
                let seen_at = started.elapsed();
                let event = resolve_mint(&client, &http_url, &tx.signature, &decoder).await;
                let mint = event.map_or_else(
                    || "<mint not resolved>".to_string(),
                    |m| bs58::encode(m).into_string(),
                );
                println!(
                    "\n🚀 LAUNCH  slot {}  t+{:.1}s\n   sig   {}\n   ix    {}\n   mint  {}",
                    slot,
                    seen_at.as_secs_f64(),
                    tx.signature,
                    name,
                    mint
                );
                launches.push(Launch {
                    slot,
                    at: seen_at,
                    signature: tx.signature.clone(),
                    mint: event.map(|m| bs58::encode(m).into_string()),
                    instruction: name.to_string(),
                });
            }
            PumpActivity::Trade(_) => trades += 1,
            PumpActivity::Other(_) => others += 1,
        }

        if last_status.elapsed() >= STATUS_EVERY {
            last_status = Instant::now();
            println!(
                "   … t+{:.0}s  launches {} | trades {} | other {} | failed {} | unparsed {}",
                started.elapsed().as_secs_f64(),
                launches.len(),
                trades,
                others,
                failed,
                malformed
            );
        }
    }

    report(
        started, &launches, trades, others, failed, malformed, &names,
    );
    Ok(())
}

/// A launch seen during the run.
struct Launch {
    slot: u64,
    at: Duration,
    signature: String,
    mint: Option<String>,
    instruction: String,
}

/// Reads the mint out of a launch transaction.
///
/// Retried, because of a measured behaviour that looks like a bug and is not:
/// the log notification arrives at `confirmed`, but `getTransaction` frequently
/// returns a **null result** for a transaction that has not been indexed yet.
/// A single attempt resolved none of twenty-one consecutive launches. Retrying
/// for a couple of seconds resolves essentially all of them — which is the real
/// shape of the latency budget on a log-based feed, and precisely what a paid
/// gRPC feed exists to remove.
async fn resolve_mint(
    client: &reqwest::Client,
    url: &str,
    signature: &str,
    decoder: &PumpFunDecoder,
) -> Option<[u8; 32]> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTransaction",
        "params": [signature, {
            "encoding": "jsonParsed",
            "maxSupportedTransactionVersion": 1
        }]
    });
    for attempt in 0..MINT_FETCH_ATTEMPTS {
        if let Ok(response) = client.post(url).json(&body).send().await {
            if let Ok(value) = response.json::<serde_json::Value>().await {
                if let Some(parsed) = value.get("result").and_then(|r| r.get("transaction")) {
                    if let Some(mint) = mint_from_parsed(parsed, *decoder) {
                        return Some(mint);
                    }
                }
            }
        }
        // Growing gap: the first retry usually lands, later ones are for the
        // rare transaction that stays unindexed for a while.
        tokio::time::sleep(Duration::from_millis(
            MINT_FETCH_BASE_DELAY_MS * (u64::from(attempt) + 1),
        ))
        .await;
    }
    None
}

/// Attempts before giving up on a mint.
const MINT_FETCH_ATTEMPTS: u32 = 4;

/// First retry delay, multiplied by attempt number.
const MINT_FETCH_BASE_DELAY_MS: u64 = 250;

/// Extracts the mint from a parsed transaction, if it holds a launch.
fn mint_from_parsed(parsed: &serde_json::Value, decoder: PumpFunDecoder) -> Option<[u8; 32]> {
    let message = parsed.get("message")?;
    let list = message.get("instructions")?.as_array()?;

    let mut ixs: Vec<RawInstructionData> = Vec::with_capacity(list.len());
    for ix in list {
        let program = ix.get("programId").and_then(|p| p.as_str()).unwrap_or("");
        let data = match ix.get("data") {
            Some(serde_json::Value::Array(items)) => items
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let accounts = ix
            .get("accounts")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        ixs.push(RawInstructionData {
            program: program.to_string(),
            data,
            accounts,
        });
    }
    if ixs.is_empty() {
        return None;
    }

    let programs: Vec<[u8; 32]> = ixs.iter().map(|i| base32(i.program.as_bytes())).collect();
    let accounts: Vec<Vec<[u8; 32]>> = ixs
        .iter()
        .map(|i| i.accounts.iter().map(|a| base32(a.as_bytes())).collect())
        .collect();
    let payloads: Vec<Vec<u8>> = ixs.iter().map(|i| decode_ix_data(&i.data)).collect();

    let raw_ixes: Vec<RawInstruction<'_>> = programs
        .iter()
        .zip(accounts.iter())
        .zip(payloads.iter())
        .map(|((p, a), d)| RawInstruction {
            program_id: *p,
            data: d.as_slice(),
            accounts: a.as_slice(),
        })
        .collect();

    let raw = RawTransaction {
        signature: [0; 64],
        slot: 0,
        instructions: &raw_ixes,
    };
    decoder.decode_launch(&raw).map(|e| e.mint)
}

/// One instruction as `jsonParsed` reports it.
struct RawInstructionData {
    program: String,
    data: String,
    accounts: Vec<String>,
}

/// Decodes a `jsonParsed` instruction payload.
///
/// ## It is base58, not base64
///
/// This cost an hour and is worth writing down. Helius returns
/// `jsonParsed` instruction `data` as a **base58** string, while the Solana RPC
/// specification says `json` encoding returns `[base64, "base64"]`. Both are
/// true, and they are different fields of the same response object.
///
/// The evidence is arithmetical and does not depend on reading any source:
///
/// | observed base58 length | base58 size | chapter 2's captured `data_len` |
/// |---|---|---|
/// | 192 chars | ~140 bytes | 140 (`create_v2`) |
/// | 33 chars | ~24 bytes | 24 (`buy_v2`) |
///
/// A base64 decoder on this input produces a plausible non-empty byte string and
/// therefore **no error at all** — it simply never matches a discriminator, so
/// every launch looks like "mint not resolved".
///
/// Both shapes are still accepted, because a provider is free to switch and a
/// bot that hard-codes one encoding breaks silently when it does.
fn decode_ix_data(data: &str) -> Vec<u8> {
    let candidate = data.split('"').nth(1).unwrap_or(data).trim();
    if candidate.is_empty() {
        return Vec::new();
    }
    if let Ok(bytes) = bs58::decode(candidate).into_vec() {
        return bytes;
    }
    // Fall back to base64 in case the field is the specified array form after
    // all, or the provider changed its mind.
    base64_decode(candidate)
}

/// Base58 into 32 bytes, or all zeroes when the input is not a pubkey.
///
/// A zeroed key is a safe placeholder here: the decoder compares program ids,
/// and a garbage program id simply means that instruction is ignored rather than
/// misattributed.
fn base32(input: &[u8]) -> [u8; 32] {
    bs58::decode(input)
        .into_vec()
        .ok()
        .and_then(|v| {
            if v.len() == 32 {
                Some(<[u8; 32]>::try_from(v).unwrap_or([0; 32]))
            } else {
                None
            }
        })
        .unwrap_or([0; 32])
}

/// Minimal base64 decoder, to keep the binary's dependency surface small.
fn base64_decode(input: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0_u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let Some(value) = TABLE.iter().position(|c| *c == byte) else {
            continue;
        };
        buffer = (buffer << 6) | u32::from(value as u8);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    out
}

/// Parses `--seconds`, defaulting to 90.
fn parse_seconds() -> u64 {
    let args: Vec<String> = std::env::args().collect();
    let mut value = 90;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--seconds" || args[index] == "-s" {
            if let Some(next) = args.get(index + 1) {
                value = next.parse().unwrap_or(value);
            }
        }
        index += 1;
    }
    value
}

/// Prints the run summary.
///
/// Launch rate is the headline: it is the number that decides whether 0.02 SOL
/// per launch is a strategy or a donation.
fn report(
    started: Instant,
    launches: &[Launch],
    trades: u64,
    others: u64,
    failed: u64,
    malformed: u64,
    names: &std::collections::BTreeMap<String, u64>,
) {
    let elapsed = started.elapsed().as_secs_f64();
    println!("\n{}", "═".repeat(64));
    println!("📊 RUN SUMMARY — {elapsed:.0}s");
    println!("{}", "═".repeat(64));
    println!("launches detected : {}", launches.len());
    println!("trades seen       : {trades}");
    println!("other pump ixs    : {others}");
    println!("failed tx ignored : {failed}");
    println!("unparsed messages : {malformed}");
    if elapsed > 0.0 {
        println!(
            "launch rate       : {:.2} per minute",
            launches.len() as f64 / elapsed * 60.0
        );
    }
    if !names.is_empty() {
        println!("\ninstruction census:");
        // Sorted by frequency, because that ordering is what identifies the
        // handlers worth reverse-engineering next. Sorting by name — the
        // default for a `BTreeMap` — would put the common cases in a random
        // place and hide the finding.
        let mut ranked: Vec<(&String, &u64)> = names.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (name, count) in ranked {
            println!("   {count:>8}  {name}");
        }
    }
    if !launches.is_empty() {
        println!("\nlaunches:");
        for launch in launches {
            let mint = launch.mint.as_deref().unwrap_or("<unresolved>");
            println!(
                "   slot {}  t+{:.1}s  {} via {}  mint {}",
                launch.slot,
                launch.at.as_secs_f64(),
                &launch.signature[..launch.signature.len().min(20)],
                launch.instruction,
                mint
            );
        }
    }
    println!("\n💰 traded: nothing. This tool detects; it does not execute.");
    println!("{}", "═".repeat(64));
}
