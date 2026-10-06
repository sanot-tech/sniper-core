//! 🔬 `risk-probe` — measure the real risk base rates before filtering.
//!
//! ## Why measure instead of writing filters
//!
//! Chapter 6's job is to decide which launches to refuse. A filter written from
//! intuition is a guess about a population nobody has sampled, and the two ways
//! it can be wrong are both expensive:
//!
//! * too permissive, and real honeypots get bought;
//! * too strict, and genuine launches are refused, which looks exactly like a
//!   quiet market.
//!
//! So this tool samples the live population and reports what is actually
//! there: how often a mint authority survives, how often a Token-2022 extension
//! appears, what the real supply is. Thresholds then follow the data.
//!
//! ## What it does
//!
//! 1. Subscribes to pump.fun logs and collects launches (no RPC cost — the
//!    WebSocket pushes them).
//! 2. Resolves each launch's mint and creator through one `getTransaction`,
//!    retried because a fresh launch is not yet indexed.
//! 3. Batches the mints through `getMultipleAccounts`, 100 at a time, which is
//!    one request per hundred launches.
//!
//! Run: `cargo risk-probe -- --seconds 600`
//!
//! ## What it does not do
//!
//! It does not trade, and it does not decide anything yet. It produces the
//! distribution that chapter 6's filters will be built against.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use sniper_core::stream::logs::{LogNotification, PumpActivity};
use sniper_core::venue::pumpfun::{PumpFunDecoder, RawInstruction, RawTransaction};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Mints per `getMultipleAccounts` request. The RPC caps this at 100.
const BATCH: usize = 100;

/// Launches collected before a batch is flushed.
const BATCH_TRIGGER: usize = 100;

/// A launch, with everything needed to judge it.
#[derive(Debug, Clone)]
struct Launch {
    slot: u64,
    signature: String,
    mint: String,
    creator: String,
    ix: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = parse_seconds();
    let key = std::env::var("HELIUS_API_KEY")
        .map_err(|_| "| 🛑 set HELIUS_API_KEY (read from the environment, never from git)")?;
    let ws_url = format!("wss://mainnet.helius-rpc.com/?api-key={key}");
    let http_url = format!("https://mainnet.helius-rpc.com/?api-key={key}");

    println!("🔬 pump.fun risk base-rate probe");
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

    let mut launches: Vec<Launch> = Vec::new();
    let mut unresolved = 0_u64;
    let mut rejected = 0_u64;

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
            rejected += 1;
            continue;
        }
        let Some(PumpActivity::Launch(name)) = tx.pump_activity() else {
            continue;
        };

        match resolve(&client, &http_url, &tx.signature, &decoder).await {
            Some((mint, creator)) => {
                launches.push(Launch {
                    slot: note.params.result.context.slot,
                    signature: tx.signature.clone(),
                    mint,
                    creator,
                    ix: name.to_string(),
                });
                if launches.len() % BATCH_TRIGGER == 0 {
                    println!(
                        "   … t+{:.0}s  {} launches collected",
                        started.elapsed().as_secs_f64(),
                        launches.len()
                    );
                }
            }
            None => unresolved += 1,
        }
    }

    println!(
        "\n   collected {} launches, {} unresolved, {} failed transactions ignored\n",
        launches.len(),
        unresolved,
        rejected
    );

    let (census, samples) = inspect_mints(&client, &http_url, &launches).await;
    report(&launches, &census, started.elapsed());
    let out = Path::new("fixtures").join("risk").join("census.json");
    write_census(&out, seconds, launches.len(), &samples);
    Ok(())
}

/// Reads mint and creator out of a launch transaction.
///
/// Both come from the same call, and both are positions the decoder already
/// knows, so nothing is parsed twice. Retried: a launch seen on the log feed is
/// frequently not yet indexed when the first request lands.
async fn resolve(
    client: &reqwest::Client,
    url: &str,
    signature: &str,
    decoder: &PumpFunDecoder,
) -> Option<(String, String)> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getTransaction",
        "params": [signature, {
            "encoding": "jsonParsed", "maxSupportedTransactionVersion": 1
        }]
    });
    for attempt in 0_u64..5 {
        if let Ok(response) = client.post(url).json(&body).send().await {
            if let Ok(value) = response.json::<serde_json::Value>().await {
                if let Some(tx) = value.get("result").and_then(|r| r.get("transaction")) {
                    if let Some((mint, creator)) = mint_and_creator(tx, *decoder) {
                        return Some((mint, creator));
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
    }
    None
}

/// Extracts mint and creator from a parsed transaction.
fn mint_and_creator(tx: &serde_json::Value, decoder: PumpFunDecoder) -> Option<(String, String)> {
    let message = tx.get("message")?;
    let list = message.get("instructions")?.as_array()?;

    let mut programs: Vec<[u8; 32]> = Vec::new();
    let mut accounts: Vec<Vec<[u8; 32]>> = Vec::new();
    let mut payloads: Vec<Vec<u8>> = Vec::new();
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
    ))
}

/// What a mint account actually contains, as measured.
///
/// Kept separate from `risk::MintFacts` for one reason: this carries the raw
/// extension *names* off the chain, which is what the census fixture needs.
/// `as_filter_input` converts to the shipping type, so the filter still judges
/// real data rather than a lookalike.
// The bools are a flat list because they mirror the account's fields one for one;
// grouping them would obscure which authority each one refers to.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone)]
struct MintFacts {
    owner: String,
    mint_authority: bool,
    freeze_authority: bool,
    decimals: Option<u8>,
    supply: Option<u64>,
    extensions: Vec<String>,
    metadata_update_authority: bool,
    missing: bool,
}

impl MintFacts {
    /// Converts to the type the shipping filter consumes.
    fn as_filter_input(&self, creator_launches: u32) -> sniper_core::risk::MintFacts {
        let mut extensions = sniper_core::risk::Extensions::NONE;
        for name in &self.extensions {
            extensions.insert_named(name);
        }
        sniper_core::risk::MintFacts {
            program: sniper_core::risk::TokenProgram::from_program_id(&self.owner),
            mint_authority_live: self.mint_authority,
            freeze_authority_live: self.freeze_authority,
            decimals: self.decimals.unwrap_or(0),
            supply: self.supply.unwrap_or(0),
            extensions,
            metadata_mutable: self.metadata_update_authority,
            creator_launches,
        }
    }
}

/// Fetches mint accounts in batches and tallies what they contain.
async fn inspect_mints(
    client: &reqwest::Client,
    url: &str,
    launches: &[Launch],
) -> (Vec<MintFacts>, Vec<serde_json::Value>) {
    let mut facts_out: Vec<MintFacts> = Vec::with_capacity(launches.len());
    let mut raw_samples: Vec<serde_json::Value> = Vec::with_capacity(launches.len());
    let mut accepted = 0_usize;
    let mut refusals: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let filter = sniper_core::risk::RiskFilter::new();
    let mut saw_danger = 0_usize;
    // Creator cadence is a property of the window, not of any single mint, so it is
    // counted across the whole sample before judging starts.
    let mut creator_counts: std::collections::BTreeMap<&str, u32> =
        std::collections::BTreeMap::new();
    for launch in launches {
        *creator_counts.entry(launch.creator.as_str()).or_insert(0) += 1;
    }

    for chunk in launches.chunks(BATCH) {
        let keys: Vec<&str> = chunk.iter().map(|l| l.mint.as_str()).collect();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getMultipleAccounts",
            "params": [keys, { "encoding": "jsonParsed" }]
        });
        let (values, _raw): (Vec<Option<MintFacts>>, Vec<serde_json::Value>) =
            match client.post(url).json(&body).send().await {
                Ok(response) => match response.json::<serde_json::Value>().await {
                    Ok(value) => {
                        if let Some(error) = value.get("error") {
                            println!(
                                "   ⚠️  getMultipleAccounts rejected a batch of {}: {error}",
                                chunk.len()
                            );
                        }
                        let raw = value
                            .get("result")
                            .and_then(|r| r.get("value"))
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let parsed = raw.iter().map(parse_mint).collect();
                        (parsed, raw)
                    }
                    Err(e) => {
                        println!("   ⚠️  batch of {} unparseable: {e}", chunk.len());
                        (Vec::new(), Vec::new())
                    }
                },
                Err(e) => {
                    println!("   ⚠️  batch of {} failed: {e}", chunk.len());
                    (Vec::new(), Vec::new())
                }
            };
        for (index, launch) in chunk.iter().enumerate() {
            let facts = values.get(index).cloned().flatten().unwrap_or(MintFacts {
                missing: true,
                ..MintFacts::default()
            });
            if !facts.missing {
                // Judge with the shipping filter rather than a probe-local copy.
                // Measuring with different code would answer a different
                // question than the one chapter 6 actually asks.
                let launches_by_creator = creator_counts
                    .get(launch.creator.as_str())
                    .copied()
                    .unwrap_or(1);
                match filter.judge(&facts.as_filter_input(launches_by_creator)) {
                    sniper_core::risk::Verdict::Accept => accepted += 1,
                    sniper_core::risk::Verdict::Reject(flags) => {
                        for flag in flags {
                            *refusals.entry(format!("{flag:?}")).or_insert(0) += 1;
                        }
                    }
                }
                if facts.mint_authority
                    || facts.freeze_authority
                    || facts.extensions.iter().any(|n| {
                        matches!(
                            n.as_str(),
                            "transferFee"
                                | "transferHook"
                                | "permanentDelegate"
                                | "defaultAccountState"
                                | "nonTransferable"
                        )
                    })
                {
                    saw_danger += 1;
                }
            }
            raw_samples.push(census_sample(launch, &facts));
            facts_out.push(facts);
        }
    }
    println!(
        "\n   filter verdict: {accepted} accepted of {} inspected, {saw_danger} carried a live danger",
        facts_out.iter().filter(|f| !f.missing).count()
    );
    for (reason, count) in refusals.iter().rev() {
        println!("      refused {count:>4}  {reason}");
    }
    (facts_out, raw_samples)
}

/// Serialises one launch plus its mint facts, for the chapter-6 fixture.
fn census_sample(launch: &Launch, facts: &MintFacts) -> serde_json::Value {
    serde_json::json!({
        "signature": launch.signature,
        "slot": launch.slot,
        "instruction": launch.ix,
        "creator": launch.creator,
        "mint": launch.mint,
        "owner": facts.owner,
        "mint_authority_live": facts.mint_authority,
        "freeze_authority_live": facts.freeze_authority,
        "decimals": facts.decimals,
        "supply": facts.supply,
        "extensions": facts.extensions,
        "metadata_mutable": facts.metadata_update_authority,
        "missing": facts.missing,
    })
}

/// Writes the census where chapter 6's tests expect it.
///
/// Without this the risk filters would be tested against invented numbers, and
/// an invented base rate is the one thing a threshold must never be built on.
fn write_census(path: &Path, seconds: u64, launches: usize, samples: &[serde_json::Value]) {
    let document = serde_json::json!({
        "captured": chrono_today(),
        "seconds": seconds,
        "launches": launches,
        "mints_inspected": samples.iter().filter(|s| s["missing"] == false).count(),
        "samples": samples,
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(
        path,
        serde_json::to_string_pretty(&document).unwrap_or_default(),
    ) {
        Ok(()) => println!("   💾 census written to {}", path.display()),
        Err(e) => println!("   ⚠️  could not write census: {e}"),
    }
}

/// Today's date, read from the system clock.
///
/// No date crate for one string. The alternative is a dependency whose only
/// purpose is to avoid calling `SystemTime`, which is not a trade worth making.
fn chrono_today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Days since epoch, then a civil-from-days conversion (Howard Hinnant's
    // algorithm) — small, exact, and no dependency.
    // `try_into` rather than `as`: the era arithmetic below is signed, and a
    // truncating cast on a machine with a wrong clock would produce a plausible
    // wrong date instead of a visible error.
    let days: i64 = (seconds / 86_400).try_into().unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

/// Reads one mint account.
///
/// ## Two shapes, one thing
///
/// The same account comes back in two different envelopes depending on the
/// method that fetched it:
///
/// ```text
///   getAccountInfo      → value: { account: { lamports, data, owner, … } }
///   getMultipleAccounts → value: [ { lamports, data, owner, … }, … ]
/// ```
///
/// Reading only the wrapped form made every one of 144 live mints report as
/// "account missing" — the batch succeeded, the parse quietly produced nothing,
/// and the census came out as a clean sheet of zeros. A statistic of zero is the
/// most convincing wrong answer available, which is why both shapes are handled
/// explicitly rather than one being assumed.
fn parse_mint(entry: &serde_json::Value) -> Option<MintFacts> {
    let account = match entry.get("account") {
        Some(wrapped) => wrapped,
        // Inline form: the entry *is* the account.
        None => entry,
    };
    let info = account.get("data")?.get("parsed")?.get("info")?;
    let mut facts = MintFacts {
        owner: account.get("owner")?.as_str()?.to_string(),
        mint_authority: info.get("mintAuthority").is_some_and(|v| !v.is_null()),
        freeze_authority: info.get("freezeAuthority").is_some_and(|v| !v.is_null()),
        decimals: info
            .get("decimals")
            .and_then(serde_json::Value::as_u64)
            .and_then(|d| u8::try_from(d).ok()),
        supply: info
            .get("supply")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok()),
        missing: false,
        ..MintFacts::default()
    };
    if let Some(list) = info.get("extensions").and_then(|e| e.as_array()) {
        for ext in list {
            let Some(kind) = ext.get("extension").and_then(|e| e.as_str()) else {
                continue;
            };
            facts.extensions.push(kind.to_string());
            if kind == "tokenMetadata" {
                facts.metadata_update_authority = ext
                    .get("state")
                    .and_then(|s| s.get("updateAuthority"))
                    .is_some_and(|v| !v.is_null());
            }
        }
    }
    Some(facts)
}

/// Prints the census. Percentages matter more than counts: they are what a
/// threshold has to be chosen against.
///
/// One long block on purpose: every section is a slice of the same population,
/// and splitting them would let a reader compare numbers from different
/// denominators.
#[allow(clippy::too_many_lines)]
fn report(launches: &[Launch], census: &[MintFacts], elapsed: Duration) {
    let total = census.len();
    if total == 0 {
        println!("no mints inspected — nothing to report");
        return;
    }

    let counted =
        |predicate: &dyn Fn(&MintFacts) -> bool| census.iter().filter(|f| predicate(f)).count();

    let missing = counted(&|f| f.missing);
    let live = total - missing;
    let mint_auth = counted(&|f| f.mint_authority);
    let freeze_auth = counted(&|f| f.freeze_authority);
    let token2022 = counted(&|f| f.owner == TOKEN_2022);
    let mutable_meta = counted(&|f| f.metadata_update_authority);

    let mut extensions: BTreeMap<String, usize> = BTreeMap::new();
    for facts in census {
        for ext in &facts.extensions {
            *extensions.entry(ext.clone()).or_insert(0) += 1;
        }
    }

    let mut supplies: BTreeMap<u64, usize> = BTreeMap::new();
    let mut decimal_counts: BTreeMap<u8, usize> = BTreeMap::new();
    for facts in census {
        if let Some(supply) = facts.supply {
            *supplies.entry(supply).or_insert(0) += 1;
        }
        if let Some(d) = facts.decimals {
            *decimal_counts.entry(d).or_insert(0) += 1;
        }
    }

    let percent = |count: usize| {
        if live == 0 {
            0.0
        } else {
            count as f64 * 100.0 / live as f64
        }
    };

    println!("{}", "═".repeat(70));
    println!(
        "🔬 RISK BASE RATES — {} launches over {:.0}s",
        total,
        elapsed.as_secs_f64()
    );
    println!(
        "   launch rate {:.1}/min",
        total as f64 / elapsed.as_secs_f64() * 60.0
    );
    println!("{}", "═".repeat(70));
    println!("mints inspected      : {live} (account missing: {missing})");
    println!();
    println!("🔴 INSTAWN-UNPURCHASEABLE");
    println!(
        "   mint authority live: {:>4}  ({:.1}%)  ← can print supply, instant rug",
        mint_auth,
        percent(mint_auth)
    );
    println!(
        "   freeze authority   : {:>4}  ({:.1}%)  ← can freeze your ATA, cannot sell",
        freeze_auth,
        percent(freeze_auth)
    );
    println!(
        "   mutable metadata   : {:>4}  ({:.1}%)",
        mutable_meta,
        percent(mutable_meta)
    );
    println!();
    println!("🟡 TOKEN-2022 SHARE");
    println!(
        "   Token-2022 mints   : {:>4}  ({:.1}%)",
        token2022,
        percent(token2022)
    );
    if !extensions.is_empty() {
        println!("   extensions seen:");
        for (kind, count) in extensions.iter().rev() {
            println!("      {count:>5}  ({:.1}%)  {kind}", percent(*count));
        }
    }
    println!();
    println!("🟢 SUPPLY / DECIMALS");
    if supplies.is_empty() {
        println!("   (no supply parsed)");
    } else {
        for (supply, count) in supplies.iter().rev() {
            println!(
                "   supply {supply:<22} {count:>5}  ({:.1}%)",
                percent(*count)
            );
        }
    }
    if !decimal_counts.is_empty() {
        for (d, count) in &decimal_counts {
            println!("   decimals {d:<21} {count:>5}  ({:.1}%)", percent(*count));
        }
    }

    println!();
    println!("👥 TOP CREATORS (serial ruggers show up here first)");
    let mut by_creator: BTreeMap<&str, usize> = BTreeMap::new();
    for launch in launches {
        *by_creator.entry(launch.creator.as_str()).or_insert(0) += 1;
    }
    let mut ranked: Vec<(&&str, &usize)> = by_creator.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (creator, count) in ranked.iter().take(8) {
        println!("   {count:>4} launches  {creator}");
    }
    let repeat = ranked.iter().filter(|(_, c)| **c > 1).count();
    println!(
        "   creators appearing more than once: {repeat} of {}",
        ranked.len()
    );

    println!("{}", "═".repeat(70));
    println!("💰 traded: nothing. Read-only probe.");
    println!("{}", "═".repeat(70));
}

/// Token-2022 program id.
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Decodes instruction data: base58 first, base64 as a fallback.
///
/// Helius returns `jsonParsed` instruction `data` base58, which the RPC
/// specification says should be base64. A base64 decoder accepts this input,
/// returns a plausible non-empty byte string and raises nothing — it just never
/// matches a discriminator, so every launch looks unresolved.
fn decode_ix_data(data: &str) -> Vec<u8> {
    let candidate = data.split('"').nth(1).unwrap_or(data).trim();
    if candidate.is_empty() {
        return Vec::new();
    }
    if let Ok(bytes) = bs58::decode(candidate).into_vec() {
        return bytes;
    }
    base64_decode(candidate)
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

/// Parses `--seconds`, defaulting to 300.
fn parse_seconds() -> u64 {
    let args: Vec<String> = std::env::args().collect();
    let mut value = 300;
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
