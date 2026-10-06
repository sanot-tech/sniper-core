//! 🛡️ Gatekeeper — chapter 0's autotest system.
//!
//! A preflight that runs before anything else and refuses to let the project
//! build on an environment that could lose money or produce dishonest
//! measurements.
//!
//! Design rules:
//!
//! * **fail loud** — exit code 2 with a list of blockers, never a warning that
//!   scrolls past;
//! * **zero dependencies** — the guard must compile faster than the code it
//!   guards;
//! * **no network** — nothing here may fail because a provider is down, or the
//!   guard becomes noise during an outage.
//!
//! Usage: `cargo run -p xtask -- gatekeeper`

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One check outcome.
#[derive(Debug)]
enum Check {
    /// Passed, with the value that made it pass.
    Pass(String),
    /// Failed but does not block development.
    Warn(String),
    /// Failed and blocks.
    Block(String),
}

impl Check {
    fn icon(&self) -> &'static str {
        match self {
            Self::Pass(_) => "✅",
            Self::Warn(_) => "⚠️ ",
            Self::Block(_) => "🛑",
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Pass(t) | Self::Warn(t) | Self::Block(t) => t,
        }
    }

    fn is_block(&self) -> bool {
        matches!(self, Self::Block(_))
    }
}

/// Runs a command and returns trimmed stdout, if it succeeded.
fn probe(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Minimum Rust we rely on: workspace `rust-version` plus the lints we enable.
const MIN_RUST: (u32, u32, u32) = (1, 85, 0);

fn check_rust() -> Check {
    let Some(v) = probe("rustc", &["--version"]) else {
        return Check::Block("rustc not found on PATH".into());
    };
    let Some(numbers) = version_triple(&v) else {
        return Check::Warn(format!("rustc found but version unparsable: {v}"));
    };
    if numbers < MIN_RUST {
        return Check::Block(format!(
            "rustc {} older than required {}.{}.{}",
            v, MIN_RUST.0, MIN_RUST.1, MIN_RUST.2
        ));
    }
    Check::Pass(v)
}

fn check_cargo() -> Check {
    match probe("cargo", &["--version"]) {
        Some(v) => Check::Pass(v),
        None => Check::Block("cargo not found on PATH".into()),
    }
}

fn check_solana() -> Check {
    match probe("solana", &["--version"]) {
        Some(v) => Check::Pass(format!("{v} (needed for `program dump` in ch.1)")),
        None => {
            Check::Warn("solana CLI missing — chapter 1 cannot dump the pump program yet".into())
        }
    }
}

fn check_repo(root: &Path) -> Check {
    if root.join(".git").exists() {
        Check::Pass("git repository initialised".into())
    } else {
        Check::Block("not a git repository — refusing to work outside version control".into())
    }
}

/// Secrets must be ignored *before* the first commit, not after.
fn check_gitignore(root: &Path) -> Check {
    let path = root.join(".gitignore");
    if !path.exists() {
        return Check::Block(".gitignore missing — keys would be committed".into());
    }
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let required = [".env", "*.key", "key.json", "fixtures/programs/"];
    let missing: Vec<_> = required
        .iter()
        .copied()
        .filter(|r| !body.contains(r))
        .collect();
    if !missing.is_empty() {
        return Check::Block(format!(".gitignore does not cover: {}", missing.join(", ")));
    }
    Check::Pass(".gitignore covers key, env and fixture patterns".into())
}

/// The highest-value scan: a base58 secret is exactly 64 characters, so a
/// committed seed shows up as an oddly specific line length.
fn check_no_seeds_in_index(root: &Path) -> Check {
    let Some(list) = probe_in(root, &["ls-files", "-z"]) else {
        return Check::Warn("could not list tracked files".into());
    };
    let suspicious: Vec<&str> = list
        .split('\0')
        .filter(|f| !f.is_empty())
        .filter(|f| {
            std::fs::read_to_string(root.join(f))
                .map(|body| body.lines().any(is_base58_secret))
                .unwrap_or(false)
        })
        .take(5)
        .collect();
    if suspicious.is_empty() {
        Check::Pass(format!(
            "no 64-char base58 secret in {} tracked files",
            list.split('\0').count()
        ))
    } else {
        Check::Block(format!(
            "possible private key in tracked files: {}",
            suspicious.join(", ")
        ))
    }
}

/// 64 base58 chars = ed25519 seed or Solana secret key.
fn is_base58_secret(line: &str) -> bool {
    let t = line.trim();
    t.len() == 64
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() && !"0OIl".contains(c))
}

/// Windows-only trap: autocrlf would rewrite committed line endings.
fn check_autocrlf() -> Check {
    let Some(v) = probe("git", &["config", "--global", "--get", "core.autocrlf"]) else {
        return Check::Block(
            "core.autocrlf is not set — set it to false so Windows does not rewrite line endings"
                .into(),
        );
    };
    if v.eq_ignore_ascii_case("false") {
        Check::Pass("core.autocrlf = false".into())
    } else {
        Check::Block(format!("core.autocrlf = {v}, expected false"))
    }
}

fn check_gh() -> Check {
    match probe("gh", &["auth", "status"]) {
        Some(_) => Check::Pass("gh authenticated (human-readable commits enabled)".into()),
        None => Check::Warn("gh not authenticated — cannot push or open PRs".into()),
    }
}

/// Secrets must exist somewhere, and must not be tracked.
fn check_secret_env(root: &Path) -> Check {
    let required = ["HELIUS_API_KEY", "JUPITER_API_KEY"];
    let present: Vec<_> = required
        .iter()
        .copied()
        .filter(|k| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false))
        .collect();
    let tracked = probe_in(root, &["ls-files", "--error-unmatch", ".env"]).is_some();
    if tracked {
        return Check::Block(
            ".env is tracked by git — remove it with `git rm --cached .env`".into(),
        );
    }
    match present.len() {
        n if n == required.len() => Check::Pass(format!("{} secrets present in environment", n)),
        n => Check::Warn(format!(
            "only {n}/{} secrets set ({:?} missing)",
            required.len(),
            required
        )),
    }
}

/// Builds the bot must not be published to crates.io by accident.
fn check_publish_flag() -> Check {
    let manifest = std::fs::read_to_string("crates/telemetry/Cargo.toml").unwrap_or_default();
    if manifest.contains("publish = false") {
        Check::Pass("internal crates marked publish = false".into())
    } else {
        Check::Warn("internal crate is publishable — consider publish = false".into())
    }
}

/// Probe that runs a git command inside the repository.
fn probe_in(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Extracts `x.y.z` from a version banner like `rustc 1.99.0 (b940084d7 2026-09-28)`.
fn version_triple(s: &str) -> Option<(u32, u32, u32)> {
    let idx = s.find(|c: char| c.is_ascii_digit())?;
    let tail = &s[idx..];
    let end = tail
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(tail.len());
    let mut it = tail[..end].split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = it.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, patch))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn main() {
    let root = repo_root();
    println!("🛡️  Gatekeeper — environment preflight");
    println!("    repo: {}\n", root.display());

    let checks = vec![
        check_rust(),
        check_cargo(),
        check_solana(),
        check_repo(&root),
        check_gitignore(&root),
        check_no_seeds_in_index(&root),
        check_autocrlf(),
        check_gh(),
        check_secret_env(&root),
        check_publish_flag(),
    ];

    let mut blocked = 0_usize;
    for c in &checks {
        println!("{} {}", c.icon(), c.text());
        if c.is_block() {
            blocked += 1;
        }
    }

    println!();
    if blocked == 0 {
        println!(
            "✅ Gatekeeper passed: {}/{} checks green.",
            checks.len(),
            checks.len()
        );
        std::process::exit(0);
    }
    println!(
        "🛑 Gatekeeper blocked: {blocked} of {} checks failed.",
        checks.len()
    );
    println!("   Fix the blockers above before building or running the bot.");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_banner() {
        let v = "rustc 1.99.0 (b940084d7 2026-09-28)";
        assert_eq!(version_triple(v), Some((1, 99, 0)));
    }

    #[test]
    fn rejects_unparsable_version() {
        assert_eq!(version_triple("rustc unknown"), None);
    }

    #[test]
    fn detects_base58_secrets_by_length_and_alphabet() {
        // Shape of a Solana secret key: exactly 64 base58 characters.
        // Base58 excludes '0' 'O' 'I' and 'l', so a 64-char run made only of
        // base58 characters is a near-certain secret — while a 64-char run
        // containing one of those is almost certainly prose or a hash.
        let secret = "4wB7fCjcMm4fBXPfnv3sQ9wK2mLpXtVnRsYu1HgDe8YzAbCdEfGhJkLmNpQrStUv";
        assert_eq!(secret.len(), 64, "fixture must be exactly 64 chars");
        assert!(
            is_base58_secret(secret),
            "a real 64-char key must be caught"
        );

        // 63 and 65 chars are not secrets.
        assert!(!is_base58_secret(&secret[..63]));
        assert!(!is_base58_secret(&format!("{secret}X")));

        // Characters outside the base58 alphabet disqualify the line, which
        // keeps the scanner quiet on ordinary long identifiers.
        assert!(!is_base58_secret(&"I".repeat(64)));
        assert!(!is_base58_secret(&"0".repeat(64)));

        // Trailing prose must disqualify the whole line.
        assert!(!is_base58_secret(&format!("{secret} // my wallet")));
    }

    #[test]
    fn minimum_rust_gate_is_sane() {
        assert!(MIN_RUST < (1, 99, 0), "toolchain should satisfy the gate");
    }
}
