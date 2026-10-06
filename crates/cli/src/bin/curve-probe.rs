//! 📐 `curve-probe` — measure the bonding curve at the moment of launch.
//!
//! ## Why this is the measurement that matters
//!
//! Chapter 6 established that a launch is not structurally a trap: across 107
//! live samples every mint had revoked authorities and no dangerous extension.
//! So the danger, if there is one, is in the *curve*, and the question is
//! empirical:
//!
//! * how far along is a curve when a sniper first sees it?
//! * how much of a 0.02 SOL position does the curve still have room for?
//! * how long before the curve is `complete` and the token has left the curve?
//!
//! A filter cannot be written on any of that until it has been measured.
//!
//! ## A free gift inside the account
//!
//! `BondingCurve` carries `token_total_supply`. Chapter 3 spent a while getting
//! supply wrong — first assuming one billion from documentation, then two
//! billion from a single inspected mint — and the resolution is that the number
//! never needed guessing: it is on chain, per token, and the probe reads it.
//!
//! ## Self-checking the account positions
//!
//! The curve's own `creator` field is compared against the creator the decoder
//! read from the launch instruction. If account 1 is the curve, they match. If
//! they do not, one of the two is wrong — and both feed the trade path, so this
//! comparison is the cheapest possible proof that the account layout is right.
//!
//! Run: `cargo curve-probe -- --seconds 240`

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use pump_rust_client::accounts::decode_bonding_curve;
use sniper_core::stream::logs::{LogNotification, PumpActivity};
use sniper_core::venue::pumpfun::{PumpFunDecoder, RawInstruction, RawTransaction};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Position size the strategy commits to, in lamports.
const POSITION_LAMPORTS: u64 = 20_000_000;

/// A launch with its curve read at detection time.
///
/// Carries more than the report prints — slot, signature, mint, creator and
/// curve address all belong in a written record, and a struct that drops them
/// because today's summary happens not to use them would make that record
/// impossible to build later without re-running a live capture.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct Observed {
    slot: u64,
    mint: String,
    creator: String,
    curve: String,
    /// Detected this many seconds into the run.
    detected_at: Duration,
    /// Virtual token reserve at detection.
    virtual_token: u64,
    /// Virtual quote reserve at detection.
    virtual_quote: u64,
    /// Real token reserve at detection.
    real_token: u64,
    /// Real quote reserve at detection.
    real_quote: u64,
    /// Supply read from the account rather than assumed.
    supply: u64,
    /// Whether the curve had already graduated.
    complete: bool,
    /// Mayhem mode, a pump.fun flag that changes fee and curve behaviour.
    mayhem: bool,
    /// Whether the curve's own creator field matched what the decoder read.
    creator_matches: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = parse_seconds();
    let key = std::env::var("HELIUS_API_KEY")
        .map_err(|_| "| 🛑 set HELIUS_API_KEY (read from the environment, never from git)")?;
    let ws_url = format!("wss://mainnet.helius-rpc.com/?api-key={key}");
    let http_url = format!("https://mainnet.helius-rpc.com/?api-key={key}");

    println!("📐 pump.fun bonding-curve probe");
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

    let mut observed: Vec<Observed> = Vec::new();
    let mut curve_unread = 0_u64;
    let mut creator_mismatches = 0_u64;
    let mut curve_slots: BTreeMap<usize, usize> = BTreeMap::new();

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

        let Some((mint, creator, candidates)) =
            resolve(&client, &http_url, &tx.signature, &decoder).await
        else {
            curve_unread += 1;
            continue;
        };

        let Some((snapshot, curve_index)) = read_curve(&client, &http_url, &candidates).await
        else {
            curve_unread += 1;
            continue;
        };
        *curve_slots.entry(curve_index).or_insert(0) += 1;

        let curve = candidates
            .iter()
            .find(|(i, _)| *i == curve_index)
            .map_or(String::from("<unknown>"), |(_, a)| a.clone());

        let creator_matches = snapshot.creator == creator;
        if !creator_matches {
            creator_mismatches += 1;
        }

        observed.push(Observed {
            slot: note.params.result.context.slot,
            mint,
            creator,
            curve,
            detected_at: started.elapsed(),
            virtual_token: snapshot.virtual_token,
            virtual_quote: snapshot.virtual_quote,
            real_token: snapshot.real_token,
            real_quote: snapshot.real_quote,
            supply: snapshot.supply,
            complete: snapshot.complete,
            mayhem: snapshot.mayhem,
            creator_matches,
        });
    }

    println!(
        "\n   {} curves read, {curve_unread} unreadable, {creator_mismatches} creator mismatches",
        observed.len()
    );
    println!("   which account is the curve, measured:");
    for (slot, count) in &curve_slots {
        println!("      index {slot:>2}: {count} launches");
    }
    if curve_slots.is_empty() {
        println!("      (none — the account list never produced a decodable curve)");
    }
    println!();
    report(&observed, started.elapsed());
    Ok(())
}

/// The curve fields this probe needs.
#[derive(Debug, Clone)]
struct CurveSnapshot {
    virtual_token: u64,
    virtual_quote: u64,
    real_token: u64,
    real_quote: u64,
    supply: u64,
    complete: bool,
    mayhem: bool,
    creator: String,
}

/// Resolves mint, creator and the curve address from a launch transaction.
///
/// All three come from one call and from positions the decoder already defines.
/// Retried, because a launch seen on the log feed is frequently not indexed yet.
async fn resolve(
    client: &reqwest::Client,
    url: &str,
    signature: &str,
    decoder: &PumpFunDecoder,
) -> Option<(String, String, Vec<(usize, String)>)> {
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

/// Accounts of a launch instruction, as `(index, address)`.
type AccountList = Vec<(usize, String)>;

/// Everything one launch transaction tells us.
///
/// Named rather than a bare triple, because a three-element tuple of two strings
/// and a vector is impossible to read at the call site.
type LaunchFacts = (String, String, AccountList);

/// Extracts mint, creator and the full account list from a parsed transaction.
///
/// The account list is returned whole, because which entry is the bonding curve
/// is a fact to be measured rather than a position to be assumed.
fn read_launch(tx: &serde_json::Value, decoder: PumpFunDecoder) -> Option<LaunchFacts> {
    let list = tx.get("message")?.get("instructions")?.as_array()?;
    let mut programs: Vec<[u8; 32]> = Vec::new();
    let mut accounts: Vec<Vec<[u8; 32]>> = Vec::new();
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    let mut account_texts: Vec<String> = Vec::new();
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
        account_texts.extend(accts);
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
    let candidates: Vec<(usize, String)> = account_texts
        .into_iter()
        .enumerate()
        // Skip the three addresses that are not accounts at all: the signer,
        // the system program and the pump program itself. They can never hold a
        // curve, and asking the RPC about them wastes requests.
        .filter(|(_, a)| !is_infrastructure(a))
        .collect();
    Some((
        bs58::encode(event.mint).into_string(),
        bs58::encode(event.creator).into_string(),
        candidates,
    ))
}

/// Addresses that are never a bonding curve.
fn is_infrastructure(address: &str) -> bool {
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

/// Reads and decodes the bonding curve account.
///
/// Which account is the curve was assumed from the observed `create_v2` layout,
/// and the assumption turned out to be wrong: reading account 1 as a curve
/// decoded nothing for 149 consecutive live launches. Rather than guess again,
/// every account in the launch instruction is tried and the first one that
/// deserialises as a `BondingCurve` is reported — along with its index, so the
/// position is a measured fact from that point on.
///
/// Decoded with the SDK rather than by hand: the layout is Anchor's, with an
/// eight-byte discriminator and borsh fields, and a hand-rolled offset table is
/// a bug waiting for the next account the program adds.
async fn read_curve(
    client: &reqwest::Client,
    url: &str,
    candidates: &[(usize, String)],
) -> Option<(CurveSnapshot, usize)> {
    for (index, address) in candidates {
        let Some(snapshot) = read_one_curve(client, url, address).await else {
            continue;
        };
        return Some((snapshot, *index));
    }
    None
}

/// Reads a single account and tries to decode it as a bonding curve.
async fn read_one_curve(
    client: &reqwest::Client,
    url: &str,
    address: &str,
) -> Option<CurveSnapshot> {
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
    Some(CurveSnapshot {
        virtual_token: decoded.virtual_token_reserves,
        virtual_quote: decoded.virtual_quote_reserves,
        real_token: decoded.real_token_reserves,
        real_quote: decoded.real_quote_reserves,
        supply: decoded.token_total_supply,
        complete: decoded.complete,
        mayhem: decoded.is_mayhem_mode,
        creator: bs58::encode(decoded.creator.to_bytes()).into_string(),
    })
}

/// Prints the census.
///
/// The headline is not "how many curves" but "how far along is a curve when we
/// first see it", because that number decides whether a 0.02 SOL entry is early
/// or already late.
#[allow(clippy::too_many_lines)]
fn report(observed: &[Observed], elapsed: Duration) {
    if observed.is_empty() {
        println!("no curves read — nothing to report");
        return;
    }
    let n = observed.len();

    // Progress through the curve: the share of supply already sold.
    let progress: Vec<f64> = observed
        .iter()
        .filter(|o| o.supply > 0)
        .map(|o| o.real_token as f64 / o.supply as f64 * 100.0)
        .collect();
    let mut sorted = progress.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let pct = |permille: u32| quantile(&sorted, permille);

    let complete = observed.iter().filter(|o| o.complete).count();
    let mayhem = observed.iter().filter(|o| o.mayhem).count();
    let matched = observed.iter().filter(|o| o.creator_matches).count();

    let mut supplies: BTreeMap<u64, usize> = BTreeMap::new();
    for o in observed {
        *supplies.entry(o.supply).or_insert(0) += 1;
    }

    // Price impact of the actual position size, per curve.
    //
    // Reported two ways, and the second is the one that matters:
    //
    //   * tokens bought as a share of the *virtual token reserve* — the usual
    //     "how much of the curve do I move" number, and it is tiny;
    //   * tokens bought as a share of *total supply* — the honest one. It came
    //     out around 47%, which is the finding of this whole probe.
    //
    // An earlier version printed the raw token count under a percentage label,
    // which made a 4.7e14 token purchase read as 473,020,804,842%. The number
    // was right and the label was a lie, which is worse than a wrong number.
    let mut impacts_of_virtual: Vec<f64> = observed
        .iter()
        .filter(|o| o.virtual_token > 0 && o.virtual_quote > POSITION_LAMPORTS)
        .map(|o| {
            // Share of the curve's own virtual token reserve, as a percentage.
            // Dividing by `virtual_token` is the step an earlier version omitted,
            // which printed a raw token count under a percentage label.
            let tokens = POSITION_LAMPORTS as f64 * o.virtual_token as f64 / o.virtual_quote as f64;
            tokens / o.virtual_token as f64 * 100.0
        })
        .collect();
    impacts_of_virtual.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mut impacts_of_supply: Vec<f64> = observed
        .iter()
        .filter(|o| o.supply > 0 && o.virtual_quote > POSITION_LAMPORTS)
        .map(|o| {
            POSITION_LAMPORTS as f64 * o.virtual_token as f64
                / o.virtual_quote as f64
                / o.supply as f64
                * 100.0
        })
        .collect();
    impacts_of_supply.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let at = |v: &[f64], permille: u32| quantile(v, permille);

    println!("{}", "═".repeat(72));
    println!(
        "📐 BONDING CURVE AT DETECTION — {} launches over {:.0}s",
        n,
        elapsed.as_secs_f64()
    );
    println!(
        "   launch rate {:.1}/min",
        n as f64 / elapsed.as_secs_f64() * 60.0
    );
    println!("{}", "═".repeat(72));

    println!("\n🧭 ACCOUNT POSITIONS (self-check)");
    println!(
        "   curve.creator == decoded creator : {matched}/{n} ({:.1}%)",
        matched as f64 / n as f64 * 100.0
    );
    if matched != n {
        println!("   ⚠️  MISMATCH — account 1 is not the curve, or the creator slot is wrong");
    }

    println!("\n📈 HOW FAR ALONG IS A CURVE WHEN FIRST SEEN");
    println!(
        "   sold share of supply: p10 {:.3}%  p50 {:.3}%  p90 {:.3}%  max {:.3}%",
        pct(100),
        pct(500),
        pct(900),
        sorted[sorted.len() - 1]
    );
    let untouched = observed.iter().filter(|o| o.real_token == 0).count();
    println!(
        "   curves with nothing sold yet: {untouched}/{n} ({:.1}%)",
        untouched as f64 / n as f64 * 100.0
    );
    let some_sold = observed.iter().filter(|o| o.real_token > 0).count();
    if some_sold > 0 {
        let sold: Vec<f64> = observed
            .iter()
            .filter(|o| o.real_token > 0 && o.supply > 0)
            .map(|o| o.real_token as f64 / o.supply as f64 * 100.0)
            .collect();
        println!(
            "   among those with sales: median {:.4}% of supply already gone",
            sold[sold.len() / 2]
        );
    }

    println!("\n💵 PRICE IMPACT OF 0.02 SOL");
    if impacts_of_supply.is_empty() {
        println!("   (no curve had room — virtual quote reserve too small)");
    } else {
        println!(
            "   share of VIRTUAL token reserve: p10 {:.4}%  p50 {:.4}%  p90 {:.4}%",
            at(&impacts_of_virtual, 100),
            at(&impacts_of_virtual, 500),
            at(&impacts_of_virtual, 900)
        );
        println!(
            "   share of TOTAL SUPPLY      : p10 {:.1}%  p50 {:.1}%  p90 {:.1}%",
            at(&impacts_of_supply, 100),
            at(&impacts_of_supply, 500),
            at(&impacts_of_supply, 900)
        );
        println!(
            "   → a 0.02 SOL entry buys {:.0}% of the whole token at the median",
            at(&impacts_of_supply, 500)
        );
    }

    println!("\n🚩 FLAGS");
    println!(
        "   complete (graduated off the curve): {complete}/{n} ({:.1}%)",
        complete as f64 / n as f64 * 100.0
    );
    println!(
        "   mayhem mode                         : {mayhem}/{n} ({:.1}%)",
        mayhem as f64 / n as f64 * 100.0
    );

    println!("\n📦 SUPPLY READ FROM THE ACCOUNT (never assumed)");
    for (supply, count) in supplies.iter().rev() {
        println!(
            "   {count:>4} ({:>5.1}%)  {supply}",
            *count as f64 / n as f64 * 100.0
        );
    }
    println!("   distinct values: {}", supplies.len());

    println!("\n🪙 RESERVE SHAPES");
    println!("   virtual quote reserve (lamports):");
    let mut quotes: Vec<u64> = observed.iter().map(|o| o.virtual_quote).collect();
    quotes.sort_unstable();
    println!(
        "      min {} p50 {} p90 {} max {}",
        quotes[0],
        quotes[quotes.len() / 2],
        quotes[(quotes.len() * 9 / 10).min(quotes.len() - 1)],
        quotes[quotes.len() - 1]
    );

    println!("{}", "═".repeat(72));
    println!("💰 traded: nothing. Read-only probe.");
    println!("{}", "═".repeat(72));
}

/// Index into a sorted slice at quantile `permille / 1000`.
///
/// Integer arithmetic rather than `as f64` casting: a quantile can never be
/// negative here, and a truncating cast on a float would be a way to index out
/// of bounds for no benefit.
fn quantile(sorted: &[f64], permille: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let last = sorted.len() - 1;
    let index = (last as u64 * u64::from(permille) / 1000) as usize;
    sorted[index.min(last)]
}

/// Decodes instruction data: base58 first, base64 as a fallback.
///
/// Helius returns `jsonParsed` instruction `data` base58, while the RPC
/// specification says base64. A base64 decoder accepts this input, returns a
/// plausible non-empty string and raises nothing — it simply never matches a
/// discriminator, so every launch looks unresolved.
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
