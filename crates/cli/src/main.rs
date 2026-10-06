//! рџ¦€ sniper-cli вЂ” the bot process.
//!
//! Deliberately thin. All trading logic lives in `sniper-core` so that the
//! executable stays a readable description of *how the bot is run* rather
//! than *what it does*.

use std::process::ExitCode;

use sniper_core::{Config, PositionSizing, VenueKind};
use sniper_telemetry::{Metrics, Stage, TraceRing};

/// One decision, kept for the operator to inspect after the fact.
#[derive(Debug)]
struct Decision {
    stage: Stage,
    note: &'static str,
}

fn main() -> ExitCode {
    let cfg = Config::default();

    println!("рџ¦… sniper-bot");
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

    // Fail before subscribing to anything: a bad config must be a startup
    // exit code, never a surprise at 3am with an open position.
    if let Err(e) = cfg.validate() {
        eprintln!("рџ›‘ configuration refused: {e}");
        return ExitCode::from(2);
    }

    // Telemetry wiring mirrors the archetype: always-on counters, trace ring
    // allocated once, probes compiled in only with the debug-trace feature.
    let _metrics = Metrics::new(); // wired into the pipeline in chapter 7
    let trace = TraceRing::<Decision>::new(cfg.trace_capacity);

    for venue in &cfg.venues {
        let needs_idl = venue.needs_idl_verification();
        println!(
            "   рџ”Ћ {} program={}{}",
            venue.as_str(),
            venue.program_id(),
            if needs_idl {
                "  вљ пёЏ  needs IDL verification"
            } else {
                ""
            }
        );
    }

    trace.push(Decision {
        stage: Stage::Receive,
        note: "telemetry initialised, awaiting first slot",
    });

    println!();
    println!(
        "в„№пёЏ  mode `{}` does not broadcast. Position sizing is enforced at {:.4} SOL.",
        cfg.mode,
        PositionSizing::from_lamports(cfg.position.lamports()).as_sol()
    );
    println!("   trace capacity: {} decisions", trace.capacity());
    println!("   next: chapter 1 вЂ” dump the pump program and boot the LiteSVM forge.");

    // Explicit success. A binary that falls off the end returning 0 by
    // accident is how half-finished startup paths look like healthy ones.
    ExitCode::SUCCESS
}
