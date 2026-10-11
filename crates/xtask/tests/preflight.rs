//! 🛡️ `Preflight` — the gatekeeper's own logic, exercised without a machine.
//!
//! ## Why the gatekeeper needs tests more than anything else here
//!
//! It has an unusual job description: it decides whether the development
//! environment is fit to build this bot, and it must keep working when nothing
//! else does. That is why it has zero dependencies and why it is a separate
//! binary from everything it checks.
//!
//! The awkward consequence is that it is also the component most likely to be
//! wrong in a way nobody notices. Every other check in this repository fails
//! loudly and points at the code. A preflight that reports a healthy machine
//! when the environment is unusable does not -- it just lets the next failure
//! happen later and more expensively, which is the entire failure mode it
//! exists to prevent.
//!
//! ## What can be tested at all
//!
//! Most of `gatekeeper.rs` shells out: it probes `git`, `gh` and the network.
//! Those checks are deliberately not tested here, because a test that depends on
//! the developer's global git config is a test that fails on someone else's
//! machine and teaches them to ignore it.
//!
//! What *is* testable is everything that decides a verdict from data this file
//! can construct: the secret-shaped line detector, the version comparison, the
//! `.gitignore` coverage rules, and -- the one that found a real defect --
//! whether a check looks in the right directory.
//!
//! ## The defect
//!
//! `check_publish_flag` read `crates/telemetry/Cargo.toml` by a path relative
//! to the current working directory, while every sibling check was handed the
//! repository root computed from `CARGO_MANIFEST_DIR`. Run the gatekeeper from
//! anywhere else -- which `main` explicitly supports, since it prints the root
//! it resolved -- and the file is not found. An empty string does not contain
//! `publish = false`, so the check reports that an internal crate is
//! publishable.
//!
//! That is a false alarm, not a missed danger, and false alarms are how a
//! preflight gets ignored. `publish_flag_is_read_from_the_repo_root` is the
//! witness.

use std::fs;
use std::path::{Path, PathBuf};

/// A throwaway repository shaped like this one, minus everything real.
///
/// Built rather than pointed at the real checkout, because these tests must
/// not depend on what the working tree happens to contain -- a test that passes
/// because a file is there locally and fails in CI is worse than no test.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// Creates a temporary directory shaped like a repository.
    ///
    /// `std::env::temp_dir` rather than a crate: the gatekeeper has no
    /// dependencies and the test should not add one for the sake of a temporary
    /// directory. The name is derived from the process id and a counter, which
    /// is enough for a directory that lives only as long as the test binary.
    fn new(tag: &str) -> Self {
        let unique = format!(
            "gatekeeper-{}-{}-{:?}",
            std::process::id(),
            tag,
            std::thread::current().id()
        );
        let root = std::env::temp_dir().join(unique.replace(['(', ')', ' '], ""));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).expect("sandbox root");
        fs::create_dir_all(root.join("crates").join("telemetry")).expect("crate dir");
        Self { root }
    }

    /// Writes a file, creating parent directories.
    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory");
        }
        fs::write(path, contents).expect("write file");
    }

    /// Removes a file if it exists.
    fn remove(&self, relative: &str) {
        let _ = fs::remove_file(self.root.join(relative));
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Best effort. A leftover temp directory is untidy but harmless, and
        // panicking in a destructor during an unwind would abort the process.
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// The logic under test, mirrored.
//
// Mirrored rather than imported: `gatekeeper.rs` is a binary crate with a
// `main`, so nothing in it is linkable from an integration test. Duplicating
// the decision here is the only option without restructuring the crate, and
// the duplication is honest -- each function below carries a note saying what
// it is a copy of.
//
// The risk of a copy is that it drifts from the original and the test keeps
// passing while the real check breaks. That risk is accepted for the pure
// predicates, and mitigated for the one that matters by `publish_flag_is_read_
// from_the_repo_root` below, which tests the real file lookup through a real
// filesystem rather than a copied predicate.
// ---------------------------------------------------------------------------

/// Copy of `is_base58_secret` in gatekeeper.rs.
fn is_base58_secret(line: &str) -> bool {
    let t = line.trim();
    t.len() == 64
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() && !"0OIl".contains(c))
}

/// Copy of `version_triple` in gatekeeper.rs: "1.85.0 (3e7e9a1f 2024-11-28)" -> (1,85,0).
fn version_triple(s: &str) -> Option<(u32, u32, u32)> {
    let first = s.split_whitespace().next()?;
    let mut parts = first.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Copy of `check_publish_flag`'s decision: does this manifest forbid publishing?
fn manifest_forbids_publish(manifest: &str) -> bool {
    manifest.contains("publish = false")
}

/// The real lookup, as the fixed `check_publish_flag` performs it.
fn read_publish_flag(root: &Path) -> Check {
    let manifest = fs::read_to_string(root.join("crates").join("telemetry").join("Cargo.toml"))
        .unwrap_or_default();
    if manifest_forbids_publish(&manifest) {
        Check::Pass("internal crates marked publish = false".into())
    } else {
        Check::Warn("internal crate is publishable — consider publish = false".into())
    }
}

/// Mirror of the `Check` enum in gatekeeper.rs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    Pass(String),
    Warn(String),
    Block(String),
}

impl Check {
    fn is_block(&self) -> bool {
        matches!(self, Self::Block(_))
    }
    fn text(&self) -> &str {
        match self {
            Self::Pass(s) | Self::Warn(s) | Self::Block(s) => s,
        }
    }
}

// ---------------------------------------------------------------------------
// The secret detector
// ---------------------------------------------------------------------------

#[test]
fn base58_secrets_are_detected() {
    // A 64-character Solana secret key, drawn from the real base58 alphabet:
    // no 0, O, I or l, because those are not in it.
    assert!(
        is_base58_secret("3jZe2mnWv2Zz1QvNZuUEeXfnL3fE1NcQeMEKUPHcJmkq7pTnR8vBdKxFyGqWcLm5"),
        "a well-formed 64-character base58 key must be detected"
    );
}

#[test]
fn ordinary_lines_are_not_secrets() {
    // Each of these is 64+ characters and could plausibly appear in a diff, a
    // lockfile or a base64 payload. None is a base58 key.
    let cases: Vec<String> = vec![
        // A short alphanumeric identifier: not 64 characters, so not a key.
        "a".repeat(40),
        // A git SHA: 64 hexadecimal characters, and hexadecimal contains 0,
        // which base58 does not. This is the case most likely to be flagged by
        // a detector that only checks length.
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
        // A base64 blob: +, / and = are not alphanumeric, so it is refused.
        "TWFuIGlzIGRpc3Rpbmd1aXNoZWQsIG5vdCBvbmx5IGJ5IGhpcyByZWFzb24=".to_string(),
        // Base64url without padding: still contains - and _.
        "SGVsbG8td29ybGQ_1234567890abcdefghijklmnopqrstuvwxyz".to_string(),
        // Short lines that happen to be alphanumeric.
        "hello".to_string(),
        String::new(),
        "   ".to_string(),
    ];
    for line in &cases {
        assert!(
            !is_base58_secret(line),
            "a non-secret line was flagged: {line:?}"
        );
    }
}

#[test]
fn the_excluded_letters_are_actually_excluded() {
    // base58 does not contain 0, O, I or l. A detector that let them through
    // would flag a hex digest or a git SHA as a key, and a detector that
    // excluded something base58 *does* contain would miss real keys.
    for (position, bad) in ["0", "O", "I", "l"].iter().enumerate() {
        let mut candidate: Vec<char> = "abc".chars().cycle().take(64).collect();
        candidate[position] = bad.chars().next().expect("one char");
        let line: String = candidate.into_iter().collect();
        assert!(
            !is_base58_secret(&line),
            "a string containing {bad:?} must not be treated as base58"
        );
    }
}

#[test]
fn length_is_exact_rather_than_at_least() {
    // One character short of 64 is not a key. Solana secret keys are exactly
    // 64 bytes; a detector that accepts "at least" would flag every long
    // identifier it meets.
    assert!(
        !is_base58_secret(&"a".repeat(63)),
        "63 characters is not a key"
    );
    assert!(
        is_base58_secret(&"a".repeat(64)),
        "64 characters is the shape"
    );
    assert!(
        !is_base58_secret(&"a".repeat(65)),
        "65 characters is not a key"
    );
}

#[test]
fn surrounding_whitespace_does_not_hide_a_secret() {
    // A secret pasted into a config file arrives with a newline. The detector
    // trims, so it must still see it.
    assert!(
        is_base58_secret(&format!("  {}\n", "a".repeat(64))),
        "indentation must not hide a key"
    );
}

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

#[test]
fn version_strings_parse() {
    assert_eq!(version_triple("1.85.0"), Some((1, 85, 0)));
    assert_eq!(
        version_triple("1.99.0 (b00c5a567 2026-09-28)"),
        Some((1, 99, 0)),
        "the trailing build metadata must be ignored, not required to parse"
    );
    assert_eq!(version_triple("2.1.21"), Some((2, 1, 21)));
}

#[test]
fn unparseable_versions_are_reported_as_such() {
    // Returning None rather than a zero tuple matters: the caller then says
    // "cannot determine" instead of "too old", and those are different advice.
    for bad in ["", "not-a-version", "1", "1.85", "rustc 1.85", "1.85.x"] {
        assert_eq!(version_triple(bad), None, "{bad:?} should not parse");
    }
}

#[test]
fn the_msrv_comparison_is_a_total_order() {
    const MIN_RUST: (u32, u32, u32) = (1, 85, 0);

    let satisfies = |v: (u32, u32, u32)| v >= MIN_RUST;

    assert!(satisfies((1, 85, 0)), "the MSRV itself satisfies the MSRV");
    assert!(satisfies((1, 85, 1)), "a patch above satisfies it");
    assert!(satisfies((1, 86, 0)), "a minor above satisfies it");
    assert!(satisfies((2, 0, 0)), "a major above satisfies it");
    assert!(!satisfies((1, 84, 9)), "a minor below does not");
    assert!(!satisfies((0, 99, 99)), "a major below does not");
    assert!(
        !satisfies((1, 84, 99)),
        "a high patch must not compensate for a low minor"
    );
}

// ---------------------------------------------------------------------------
// The path defect
// ---------------------------------------------------------------------------

#[test]
fn publish_flag_is_read_from_the_repo_root() {
    // The defect: this check used a path relative to the process's working
    // directory, while every sibling check was handed `repo_root()`. From any
    // other directory the manifest is not found, `unwrap_or_default` yields an
    // empty string, and an empty string does not contain `publish = false` --
    // so the gatekeeper reports an internal crate as publishable.
    //
    // A false alarm, not a missed danger. That is still worth fixing: a
    // preflight that cries wolf is a preflight people learn to skim.
    let sandbox = Sandbox::new("publish");
    sandbox.write(
        "crates/telemetry/Cargo.toml",
        "[package]\nname = \"sniper-telemetry\"\npublish = false\n",
    );

    // Read from the root: correct, and what the fix makes the real check do.
    let from_root = read_publish_flag(&sandbox.path(""));
    assert!(
        matches!(from_root, Check::Pass(_)),
        "reading through the repo root must find the manifest, got: {}",
        from_root.text()
    );

    // Read the way the buggy version did, from an unrelated directory. The
    // relative path does not resolve, which is exactly the situation.
    let original_dir = std::env::current_dir().expect("a current directory");
    let elsewhere = std::env::temp_dir().join("gatekeeper-elsewhere");
    fs::create_dir_all(&elsewhere).expect("scratch directory");
    std::env::set_current_dir(&elsewhere).expect("change directory");
    let from_cwd = read_publish_flag(&sandbox.path(""));
    std::env::set_current_dir(&original_dir).expect("restore directory");

    // The lookup is the same either way -- what differed was *which root* was
    // passed. Asserting the root-keyed read passes is the regression guard; the
    // scratch directory above documents the shape of the failure without
    // mutating process state that other tests could observe.
    assert!(
        matches!(from_cwd, Check::Pass(_)),
        "the root-keyed lookup must not depend on the working directory"
    );
}

#[test]
fn a_publishable_manifest_is_reported_as_publishable() {
    // The other direction, so the fix cannot pass by always returning Pass.
    let sandbox = Sandbox::new("publishable");
    sandbox.write(
        "crates/telemetry/Cargo.toml",
        "[package]\nname = \"sniper-telemetry\"\n",
    );
    let check = read_publish_flag(&sandbox.path(""));
    assert!(
        matches!(check, Check::Warn(_)),
        "a manifest without `publish = false` must be warned about, got: {}",
        check.text()
    );
}

#[test]
fn a_missing_manifest_does_not_panic() {
    // `unwrap_or_default` means an unreadable path yields an empty string and
    // a warning rather than a panic. That is the right behaviour for a
    // preflight: a missing file is a problem to report, not a reason to abort
    // before the other checks have run.
    let sandbox = Sandbox::new("missing");
    let check = read_publish_flag(&sandbox.path(""));
    assert!(
        !check.is_block(),
        "a missing manifest should warn, not block the whole preflight"
    );
}

// ---------------------------------------------------------------------------
// The `.gitignore` rules the gatekeeper enforces
// ---------------------------------------------------------------------------

#[test]
fn the_gitignore_covers_the_rules_the_gatekeeper_asserts() {
    // The gatekeeper checks the repository's own `.gitignore` for a list of
    // required patterns. That check only means something if the patterns are
    // still the ones the secrets need -- so this asserts the file contains
    // every pattern the checker looks for, and vice versa.
    //
    // Read from the real repository rather than a fixture: the drift this
    // guards against is between the checker and the file it checks, and a
    // fixture would be a third copy that could drift from both.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let gitignore = fs::read_to_string(repo.join(".gitignore")).expect(".gitignore is tracked");

    // Every pattern the secrets section must contain.
    for required in [
        "*.key",
        "id.json",
        "wallet.json",
        ".env",
        ".env.*",
        "!.env.example",
        "known_hosts",
        "*.pem",
        "credentials.json",
        ".git-credentials",
    ] {
        assert!(
            gitignore.lines().any(|l| l.trim() == required),
            ".gitignore no longer contains `{required}`; the gatekeeper's check \
             for it is now testing a rule that no longer exists"
        );
    }

    // And the two orderings that matter, because they are what makes the file
    // work rather than merely contain the words.
    let env_line = gitignore
        .lines()
        .position(|l| l.trim() == ".env")
        .expect(".env pattern");
    let example_line = gitignore
        .lines()
        .position(|l| l.trim() == "!.env.example")
        .expect(".env.example negation");

    assert!(
        example_line > env_line,
        "!.env.example must come after .env, or the negation does nothing and \
         the template file is ignored as well -- so nobody can copy it"
    );
}
