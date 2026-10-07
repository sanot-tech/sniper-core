//! 💰 `pnl-probe` — what a 0.02 SOL entry would actually have earned.
//!
//! ## Why this exists
//!
//! Every measurement so far describes the moment of entry. None of them says
//! what happens next, and the next part is where the money is:
//!
//! * `cargo watch` proved launches are seen in real time.
//! * `cargo risk-probe` proved none of them are structural traps.
//! * `cargo curve-probe` proved the curve is already ~78% sold and that a 0.02
//!   SOL entry is 0.1% of supply against 30 SOL of exit liquidity.
//!
//! None of that says whether the trade earns. This does, on real chain state:
//! for each detected launch the curve is sampled repeatedly, and the exit is
//! priced with the same SDK maths the program itself runs.
//!
//! ## What is reported
//!
//! For each horizon, the median and the spread of realised return, plus the
//! fraction of launches that were still profitable. The fraction matters more
//! than the median: a strategy that wins rarely and large is a different
//! strategy from one that wins often and small, and only one of them survives a
//! 0.02 SOL position size.
//!
//! ## What is deliberately not claimed
//!
//! These are *uncontested* curves. A real bot's entry competes with other
//! buyers for the same liquidity, and slippage is charged against the exit.
//! A number computed here is therefore an upper bound on what the bot could
//! have earned, and it is labelled as such.
//!
//! Run: `cargo pnl-probe -- --seconds 240`

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use pump_rust_client::accounts::decode_bonding_curve;
use sniper_core::stream::logs::{LogNotification, PumpActivity};
use sniper_core::venue::pumpfun::{PumpFunDecoder, RawInstruction, RawTransaction};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Position size, in lamports.
const POSITION: u64 = 20_000_000;

/// Horizons to price an exit at, in seconds after entry.
const HORIZONS: [u64; 5] = [5, 10, 20, 40, 80];

/// Returns by horizon, as a fraction of the position.
#[derive(Debug, Clone)]
struct Track {
    /// The mint this launch produced. Kept for the record even though today's
    /// summary only aggregates: a per-launch log is what makes a losing sample
    /// auditable instead of merely reported.
    #[allow(dead_code)]
    mint: String,
    /// Realised return at each horizon. `None` when the curve was unreadable
    /// at that point.
    returns: BTreeMap<u64, f64>,
    /// Whether the curve had already graduated when entry was priced.
    entry_complete: bool,
}

/// Entry point.
///
/// Long, and deliberately so: the read loop, the launch-time pricing and the
/// per-launch tracker are one pipeline, and splitting them would hide the
/// ordering that makes the numbers comparable.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = parse_seconds();
    let key = std::env::var("HELIUS_API_KEY")
        .map_err(|_| "| 🛑 set HELIUS_API_KEY (read from the environment, never from git)")?;
    let ws_url = format!("wss://mainnet.helius-rpc.com/?api-key={key}");
    let http_url = format!("https://mainnet.helius-rpc.com/?api-key={key}");

    println!("💰 pump.fun realised-PnL probe");
    println!("   position {POSITION} lamports (0.02 SOL), horizons {HORIZONS:?}s");
    println!("   sampling for {seconds}s — read-only, nothing is signed or sent\n");

    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url.into_client_request()?).await?;
    let subscribe = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"logsSubscribe","params":[{{"mentions":["{}"]}},{{"commitment":"confirmed"}}]}}"#,
        sniper_core::PUMP_PROGRAM_ID
    );
    socket.send(Message::Text(subscribe)).await?;

    let decoder = PumpFunDecoder::new();
    let client = reqwest::Client::new();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);

    let mut tracks: Vec<Track> = Vec::new();
    let (results_tx, mut results_rx) = tokio::sync::mpsc::unbounded_channel::<Track>();
    let mut unreadable = 0_u64;

    loop {
        if Instant::now() >= deadline {
            break;
        }
        let next = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
        let Ok(Some(Ok(message))) = next else {
            continue;
        };
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(note) = serde_json::from_str::<LogNotification>(&text) else {
            continue;
        };

        let tx = &note.params.result.value;
        if !tx.succeeded() {
            continue;
        }
        let Some(PumpActivity::Launch(_)) = tx.pump_activity() else {
            continue;
        };

        let Some((mint, _, candidates)) =
            resolve(&client, &http_url, &tx.signature, &decoder).await
        else {
            unreadable += 1;
            continue;
        };
        let Some((entry, curve)) = read_curve(&client, &http_url, &candidates).await else {
            unreadable += 1;
            continue;
        };

        // Tokens the position would buy at entry, from the SDK's own maths.
        if entry.virtual_quote <= POSITION || entry.virtual_token == 0 {
            unreadable += 1;
            continue;
        }
        let tokens = pump_rust_client::math::bonding_curve::buy_token_quote_with_sol(
            entry.virtual_quote,
            entry.virtual_token,
            POSITION,
        )
        .unwrap_or(0);
        if tokens == 0 {
            unreadable += 1;
            continue;
        }

        // Follow this launch forward in its own task.
        //
        // Deliberately not awaited inline: the horizon schedule runs to 155
        // seconds, so awaiting it in the read loop would price one launch per
        // two and a half minutes while the socket buffer filled with everything
        // that happened meanwhile. Sampling is cheap and parallel; the loop is
        // neither.
        let client = client.clone();
        let http_url = http_url.clone();
        let curve_address = curve.clone();
        let tokens_bought = tokens;
        let sender = results_tx.clone();
        tokio::spawn(async move {
            let mut returns: BTreeMap<u64, f64> = BTreeMap::new();
            let entry_complete = entry.complete;
            for horizon in HORIZONS {
                tokio::time::sleep(Duration::from_secs(horizon)).await;
                if let Some(snapshot) = read_one(&client, &http_url, &curve_address).await {
                    if snapshot.complete {
                        // Graduated: the bonding curve no longer pays out.
                        // Recorded as a total loss rather than skipped, because
                        // skipping would quietly bias the sample towards
                        // launches that happened to stay alive.
                        returns.insert(horizon, -1.0);
                    } else {
                        let sold = tokens_bought.min(u128::from(snapshot.virtual_token)).max(1);
                        let proceeds = pump_rust_client::math::bonding_curve::sell_quote(
                            snapshot.virtual_quote,
                            snapshot.virtual_token,
                            sold as u64,
                        )
                        .unwrap_or(0);
                        returns.insert(horizon, proceeds as f64 / POSITION as f64 - 1.0);
                    }
                }
            }
            let _ = sender.send(Track {
                mint,
                returns,
                entry_complete,
            });
        });
    }

    // Drain every launched tracker.
    drop(results_tx);
    while let Some(track) = results_rx.recv().await {
        tracks.push(track);
    }

    println!(
        "\n   {} launches priced, {unreadable} unreadable\n",
        tracks.len()
    );
    report(&tracks, started.elapsed());
    Ok(())
}

/// Accounts of a launch instruction, as `(index, address)`.
type AccountList = Vec<(usize, String)>;

/// Mint, creator and the full account list of a launch transaction.
type LaunchFacts = (String, String, AccountList);

/// Resolves mint and the account list from a launch transaction.
///
/// Retried: a launch seen on the log feed is frequently not yet indexed when the
/// first request lands, and a single attempt resolved none of the first five
/// launches in an earlier run.
async fn resolve(
    client: &reqwest::Client,
    url: &str,
    signature: &str,
    decoder: &PumpFunDecoder,
) -> Option<LaunchFacts> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getTransaction",
        "params": [signature, { "encoding": "jsonParsed", "maxSupportedTransactionVersion": 1 }]
    });
    for attempt in 0_u64..5 {
        if let Ok(response) = client.post(url).json(&body).send().await {
            if let Ok(value) = response.json::<serde_json::Value>().await {
                if let Some(tx) = value.get("result").and_then(|r| r.get("transaction")) {
                    if let Some(triple) = read_launch(tx, *decoder) {
                        return Some(triple);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
    }
    None
}

/// Extracts mint, creator and accounts from a parsed transaction.
fn read_launch(tx: &serde_json::Value, decoder: PumpFunDecoder) -> Option<LaunchFacts> {
    let list = tx.get("message")?.get("instructions")?.as_array()?;
    let mut programs: Vec<[u8; 32]> = Vec::new();
    let mut accounts: Vec<Vec<[u8; 32]>> = Vec::new();
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
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
        let accts: Vec<String> = ix
            .get("accounts")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        programs.push(key32(program));
        accounts.push(accts.iter().map(|a| key32(a)).collect());
        payloads.push(decode_ix_data(&data));
        texts.extend(accts);
    }
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
    let event = decoder.decode_launch(&raw)?;
    Some((
        bs58::encode(event.mint).into_string(),
        bs58::encode(event.creator).into_string(),
        texts
            .into_iter()
            .enumerate()
            .filter(|(_, a)| !infrastructure(a))
            .collect(),
    ))
}

/// Addresses that cannot hold a curve.
///
/// Skipped while sweeping for it: each one is a guaranteed miss, and asking the
/// RPC about them wastes requests on a rate-limited plan.
fn infrastructure(address: &str) -> bool {
    matches!(
        address,
        "11111111111111111111111111111111"
            | "ComputeBudget111111111111111111111111111111"
            | "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            | "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
            | "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
            | "So11111111111111111111111111111111111111112"
    ) || address == sniper_core::PUMP_PROGRAM_ID
}

/// The curve fields this probe prices against.
#[derive(Debug, Clone, Copy)]
struct Curve {
    virtual_quote: u64,
    virtual_token: u64,
    complete: bool,
}

/// Finds the curve among a launch's accounts and reads it.
async fn read_curve(
    client: &reqwest::Client,
    url: &str,
    candidates: &[(usize, String)],
) -> Option<(Curve, String)> {
    for (_, address) in candidates {
        if let Some(curve) = read_one(client, url, address).await {
            return Some((curve, address.clone()));
        }
    }
    None
}

/// Reads one account as a bonding curve.
///
/// Every account of the launch is tried until one deserialises, because the
/// curve's position was assumed wrong once already: pointing at index 1 decoded
/// nothing for 149 consecutive live launches, and measuring found index 2.
async fn read_one(client: &reqwest::Client, url: &str, address: &str) -> Option<Curve> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
        "params": [address, { "encoding": "base64" }]
    });
    let response = client.post(url).json(&body).send().await.ok()?;
    let value: serde_json::Value = response.json().await.ok()?;
    let account = value.get("result")?.get("value")?;
    let encoded = account.get("data")?.get(0)?.as_str()?;
    let bytes = base64_decode(encoded)?;
    let decoded = decode_bonding_curve(&bytes).ok()?;
    Some(Curve {
        virtual_quote: decoded.virtual_quote_reserves,
        virtual_token: decoded.virtual_token_reserves,
        complete: decoded.complete,
    })
}

/// Prints the realised-return table.
#[allow(clippy::too_many_lines)]
fn report(tracks: &[Track], elapsed: Duration) {
    if tracks.is_empty() {
        println!("no launches priced — nothing to report");
        return;
    }
    println!("{}", "═".repeat(74));
    println!(
        "💰 REALISED RETURN OF A {:.3} SOL ENTRY — {} launches",
        POSITION as f64 / 1e9,
        tracks.len()
    );
    println!("   entry at detection, exit at each horizon, fees NOT charged");
    println!("{}", "═".repeat(74));

    println!("\n📉 LAUNCHES ALREADY GRADUATED AT ENTRY");
    let complete = tracks.iter().filter(|t| t.entry_complete).count();
    println!(
        "   {complete}/{} ({:.1}%)",
        tracks.len(),
        complete as f64 / tracks.len() as f64 * 100.0
    );

    println!("\n📊 RETURN BY HORIZON  (fraction of the 0.02 SOL position)");
    println!(
        "   horizon |  samples |     p10 |     p50 |     p90 |  profitable |  worst  |   best"
    );
    for horizon in HORIZONS {
        let mut values: Vec<f64> = tracks
            .iter()
            .filter_map(|t| t.returns.get(&horizon).copied())
            .collect();
        if values.is_empty() {
            println!("   {horizon:>7}s |        - |       - |       - |       - |          - |       - |      -");
            continue;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = values.len();
        let at = |permille: u32| values[((n - 1) as u64 * u64::from(permille) / 1000) as usize];
        let winners = values.iter().filter(|v| **v > 0.0).count();
        println!(
            "   {horizon:>6}s | {n:>8} | {:>+7.2}% | {:>+7.2}% | {:>+7.2}% | {:>9.1}% | {:>+6.2}% | {:>+6.2}%",
            at(100) * 100.0,
            at(500) * 100.0,
            at(900) * 100.0,
            winners as f64 / n as f64 * 100.0,
            values[0] * 100.0,
            values[n - 1] * 100.0
        );
    }

    println!("\n🎯 WHAT THAT MEANS FOR 0.02 SOL");
    if let Some(last) = HORIZONS.last() {
        let values: Vec<f64> = tracks
            .iter()
            .filter_map(|t| t.returns.get(last).copied())
            .collect();
        if !values.is_empty() {
            let mut sorted = values.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = sorted.len();
            let median = sorted[(n - 1) / 2];
            let winners = values.iter().filter(|v| **v > 0.0).count();
            let per_100 = (winners as f64 / n as f64) * 100.0;
            println!("   at {last}s, median return {median:+.2}% on {per_100:.0}% of launches");
            println!(
                "   0.02 SOL x 100 trades at that rate ≈ {:+.4} SOL before fees",
                median * per_100 / 100.0 * 100.0
            );
            println!("   after ~2% round-trip fees, a losing median stays negative:");
            println!("   the strategy only works if the median is clearly positive.");
        }
    }

    println!("\n⚠️  UPPER BOUND. Uncontested curves, no fees, no competition for liquidity,");
    println!("    no failed broadcast. A real entry competes for the same curve and");
    println!("    pays both entry and exit fees, so the achievable number is lower.");

    println!("{}", "═".repeat(74));
    println!("💰 earned by this tool: 0.00 SOL — it observes, it does not trade");
    println!("{}", "═".repeat(74));
    let _ = elapsed;
}

/// Decodes instruction data: base58 first, base64 as a fallback.
fn decode_ix_data(data: &str) -> Vec<u8> {
    let candidate = data.split('"').nth(1).unwrap_or(data).trim();
    if candidate.is_empty() {
        return Vec::new();
    }
    if let Ok(bytes) = bs58::decode(candidate).into_vec() {
        return bytes;
    }
    base64_decode(candidate).unwrap_or_default()
}

/// Base58 into 32 bytes, or zeroes.
fn key32(input: &str) -> [u8; 32] {
    bs58::decode(input)
        .into_vec()
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .unwrap_or([0; 32])
}

/// Minimal base64 decoder, used only as a fallback.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0_u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = TABLE.iter().position(|c| *c == byte)?;
        buffer = (buffer << 6) | u32::from(value as u8);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Parses `--seconds`, defaulting to 240.
fn parse_seconds() -> u64 {
    let args: Vec<String> = std::env::args().collect();
    let mut value = 240;
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
