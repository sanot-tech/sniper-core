//! 🦈 sniper-cli — the bot process.
//!
//! Deliberately thin. All trading logic lives in `sniper-core` so that this
//! executable stays a readable description of *how the bot is run* rather than
//! *what it does*.

use std::process::ExitCode;

use sniper_core::{Config, PositionSizing, VenueKind};
use sniper_telemetry::{Metrics, Stage, TraceRing};

/// One decision, kept for the operator to inspect after the fact.
///
/// The fields are read by [`Decision::render`]; before chapter 7 there is no
/// decision flow to record yet, so the trace only gets seeded.
#[derive(Debug)]
struct Decision {
    stage: Stage,
    note: &'static str,
}

impl Decision {
    /// Single-line rendering for the operator log.
    fn render(&self) -> String {
        format!("[{}] {}", self.stage.as_str(), self.note)
    }
}

fn main() -> ExitCode {
    let cfg = Config::default();

    println!("🦅 sniper-bot");
    println!("   mode:      {}", cfg.mode);
    println!("   position:  {:.4} SOL", cfg.position.as_sol());
    println!(
        "   venues:    {}",
        cfg.venues
            .iter()
            .map(VenueKind::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("   max pos:   {}", cfg.max_open_positions);
    println!("   risk cap:  {}/10", cfg.max_risk_score);

    // Fail before subscribing to anything: a bad configuration must be a
    // startup exit code, never a surprise at 3am with an open position.
    if let Err(e) = cfg.validate() {
        eprintln!("🛑 configuration refused: {e}");
        return ExitCode::from(2);
    }

    // Telemetry wiring mirrors the archetype: always-on counters, a trace ring
    // allocated once, and probes compiled in only with the debug-trace feature.
    let _metrics = Metrics::new(); // wired into the pipeline in chapter 7
    let trace = TraceRing::<Decision>::new(cfg.trace_capacity);

    for venue in &cfg.venues {
        let needs_idl = venue.needs_idl_verification();
        println!(
            "   🔎 {} program={}{}",
            venue.as_str(),
            venue.program_id(),
            if needs_idl {
                "  ⚠️  needs IDL verification"
            } else {
                ""
            }
        );
    }

    trace.push(Decision {
        stage: Stage::Receive,
        note: "telemetry initialised, awaiting first slot",
    });

    // Drain immediately: the whole point of a bounded trace ring is that the
    // operator can read the last decisions without the hot path ever blocking.
    for decision in trace.drain() {
        println!("   📝 {}", decision.render());
    }

    println!();
    println!(
        "ℹ️  mode `{}` does not broadcast. Position sizing is enforced at {:.4} SOL.",
        cfg.mode,
        PositionSizing::from_lamports(cfg.position.lamports()).as_sol()
    );
    println!("   trace capacity: {} decisions", trace.capacity());
    println!("   next: chapter 2 - decode launch events, 0 false positives.");

    // Explicit success. A binary that falls off the end returning 0 by
    // accident is how half-finished startup paths look like healthy ones.
    ExitCode::SUCCESS
}
