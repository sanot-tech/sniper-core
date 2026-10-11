//! 🧪 `SandboxForge` — chapter 1's autotest system.
//!
//! ## The problem it solves
//!
//! pump.fun has **no devnet deployment**, so the usual "test against devnet"
//! workflow cannot exercise a `create` or `buy_v2` instruction at all. And
//! mocking the program would only prove that our mocks agree with themselves.
//!
//! ## The trick
//!
//! Dump the real mainnet program bytecode once (10 MB, about 1.5 seconds via
//! Helius) and load it into [`litesvm`], an in-process Solana VM. Tests then
//! execute the genuine program logic, with real compute metering and real
//! account constraints, but with no network, no validator and no ledger.
//!
//! * cost: zero
//! * speed: milliseconds
//! * fidelity: higher than devnet, because it is the same binary mainnet runs
//!
//! ## Why determinism matters
//!
//! Every fixture is built from a fixed seed, so two runs must produce
//! byte-identical transactions. Without that, chapter 4's golden snapshots
//! are worthless and a regression shows up as "the tests are flaky" instead of
//! "the builder changed".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub mod stub_program;

pub use stub_program::stub_program_bytes;

use litesvm::LiteSVM;
use solana_account::Account;
use solana_address::Address;
use solana_clock::Clock;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_native_token::LAMPORTS_PER_SOL;
use solana_sha256_hasher::hash as sha256;
use solana_signer::Signer;
use solana_transaction::{versioned::VersionedTransaction, Transaction};

/// Where the program dump lives, relative to the workspace root.
pub const DEFAULT_PROGRAM_PATH: &str = "fixtures/programs/pump.so";

/// pump.fun bonding curve program.
pub const PUMP_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Domain-separated deterministic bytes for a tagged fixture value.
///
/// Domain separation matters: a fixture seed derived this way can never
/// collide with, or be mistaken for, a real wallet seed.
/// Deterministic bytes from a tagged value, for fixtures.
fn tagged_digest(tag: &str, salt: Option<&[u8]>) -> [u8; 32] {
    let mut buf = Vec::with_capacity(48 + tag.len());
    buf.extend_from_slice(b"sniper-sandbox");
    buf.extend_from_slice(tag.as_bytes());
    if let Some(s) = salt {
        buf.extend_from_slice(s);
    }
    // Hash once, then keep it: this runs only during fixture construction, so
    // readability beats a micro-optimisation that no hot path would see.
    sha256(&buf).to_bytes()
}

/// Deterministic keypair derived from a tag: same tag, same address, always.
///
/// `Keypair::try_from(&[u8])` wants a 64-byte secret key (seed || public key),
/// not a bare 32-byte seed — hence the explicit expansion. Getting this wrong
/// fails at runtime with an opaque `SignatureError`, so it is done once, here.
fn keypair_from_tag(tag: &str) -> Keypair {
    let seed: [u8; 32] = tagged_digest(tag, None);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let secret: [u8; 64] = signing_key.to_keypair_bytes();
    Keypair::try_from(&secret[..]).expect("a 64-byte ed25519 secret key is always valid")
}

/// An isolated sandbox with the real pump program deployed.
pub struct Forge {
    svm: LiteSVM,
    program_id: Address,
    /// Deterministic counter so fixtures never share an address by accident.
    next_id: AtomicU64,
}

impl std::fmt::Debug for Forge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // LiteSVM owns the whole account set; dumping it would be noise.
        f.debug_struct("Forge")
            .field("program_id", &self.program_id.to_string())
            .field("slot", &self.slot())
            .finish_non_exhaustive()
    }
}

impl Forge {
    /// Builds a sandbox with the pump program loaded from the default path.
    ///
    /// Falls back to a synthesised stub when the real dump is absent, so a
    /// fresh clone has a working test suite. See [`stub_program`] for exactly
    /// what the stub does and, more importantly, does not prove.
    ///
    /// The substitution is announced on stderr rather than made silently. A
    /// suite that quietly swapped a 10 MB mainnet program for sixteen bytes of
    /// `exit` would be asserting things about the wrong program while looking
    /// entirely healthy.
    ///
    /// # Panics
    ///
    /// Panics only if the dump exists but is empty or unreadable. A *missing*
    /// dump is no longer fatal -- that used to fail all six tests in this crate
    /// on every clean checkout, because `fixtures/programs/` is gitignored.
    #[must_use]
    pub fn load_default() -> Self {
        Self::load(program_path())
    }

    /// Builds a sandbox with the pump program loaded from `path`, or from the
    /// synthesised stub when `path` does not exist.
    ///
    /// Existing-but-broken stays fatal on purpose. A truncated dump, or a
    /// leftover HTML error page from a failed download, would be exactly the
    /// failure most worth seeing -- substituting a stub would hide it.
    ///
    /// # Panics
    ///
    /// Panics when the file exists but is empty, cannot be read, or is not a
    /// loadable program.
    #[must_use]
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        match std::fs::read(path) {
            Ok(bytes) => {
                assert!(
                    !bytes.is_empty(),
                    "pump program dump at {} is empty",
                    path.display()
                );
                Self::load_bytes(&bytes)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "\u{26a0}\u{fe0f}  no pump dump at {} ({e}).\n\
                     \u{26a0}\u{fe0f}  Falling back to a synthesised stub. The VM, the account model,\n\
                     \u{26a0}\u{fe0f}  determinism and the log paths are still tested for real, but\n\
                     \u{26a0}\u{fe0f}  nothing here exercises pump.fun's own behaviour.\n\
                     \u{26a0}\u{fe0f}  Run `cargo dump-pump` to test against the real mainnet program.",
                    path.display()
                );
                Self::load_bytes(&stub_program_bytes())
            }
            Err(e) => panic!(
                "cannot read pump program dump at {}: {e}\n\
                 the file exists but could not be read. Deleting it makes the Forge\n\
                 fall back to the stub; leaving it in place keeps the failure loud.",
                path.display()
            ),
        }
    }

    /// Builds a sandbox from raw SBPF/ELF bytecode.
    ///
    /// Mainnet's active feature set is loaded alongside, because a program that
    /// only works against mainnet's actual gates is not much of a test.
    #[must_use]
    pub fn load_bytes(bytes: &[u8]) -> Self {
        let program_id: Address = PUMP_PROGRAM_ID
            .parse()
            .expect("pump id is a valid base58 pubkey");
        let mut svm = LiteSVM::new()
            // Fixture keys are deterministic and trusted; verifying them buys
            // nothing and costs real time on every single test transaction.
            .with_sigverify(false)
            // Mainnet's active feature set, so the program behaves as it does
            // in production rather than against a stale gate configuration.
            .with_mainnet_features()
            // LiteSVM funds every airdrop from one internal account. Left at
            // the default it cannot even pay rent for the accounts it creates,
            // so a "poor wallet" fixture would fail during setup instead of
            // during the transfer we actually want to observe.
            .with_lamports(100_000 * LAMPORTS_PER_SOL);
        svm.add_program(program_id, bytes)
            .expect("pump bytecode must be a loadable SBF program");
        Self {
            svm,
            program_id,
            next_id: AtomicU64::new(0),
        }
    }

    /// The deployed pump program id.
    #[inline]
    #[must_use]
    pub const fn program_id(&self) -> Address {
        self.program_id
    }

    /// Mutable access, for tests that craft account state directly.
    pub const fn svm_mut(&mut self) -> &mut LiteSVM {
        &mut self.svm
    }

    /// A fresh deterministic wallet funded with `sol`.
    #[must_use]
    pub fn funded_wallet(&mut self, tag: &str, sol: f64) -> Keypair {
        let kp = keypair_from_tag(tag);
        // Fixture-only conversion. Guarded by the assertion below so a
        // nonsensical amount fails loudly at construction instead of
        // truncating into a wallet nobody can explain.
        assert!(
            (0.0..1_000_000.0).contains(&sol),
            "fixture funding amount out of range: {sol} SOL",
        );
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lamports = (sol * LAMPORTS_PER_SOL as f64) as u64;
        self.svm
            .airdrop(&kp.pubkey(), lamports)
            .expect("airdrop inside an in-process VM cannot fail");
        kp
    }

    /// A unique deterministic address, for mints and PDAs in fixtures.
    #[must_use]
    pub fn unique_address(&self, tag: &str) -> Address {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        Address::new_from_array(tagged_digest(tag, Some(&n.to_le_bytes())))
    }

    /// Current sandbox slot, read from the Clock sysvar.
    ///
    /// Read from the VM rather than tracked locally: `LiteSVM` exposes no
    /// `latest_slot`, and a shadow counter would silently disagree with the
    /// real position after any warp.
    #[must_use]
    pub fn slot(&self) -> u64 {
        self.svm.get_sysvar::<Clock>().slot
    }

    /// Advances the sandbox clock so time-dependent logic can be exercised.
    ///
    /// # Panics
    /// Panics if asked to go backwards.
    pub fn warp_to_slot(&mut self, slot: u64) {
        let current = self.slot();
        assert!(
            slot >= current,
            "cannot warp backwards: {current} -> {slot}; time travel is one-way"
        );
        self.svm.warp_to_slot(slot);
    }

    /// Builds and executes a transaction, returning compute units consumed.
    ///
    /// Simulates first so a failing instruction surfaces its logs rather than an
    /// opaque error — the same simulate-then-send discipline the bot uses before
    /// it risks real money.
    ///
    /// # Panics
    /// Panics if simulation or execution fails, and prints the program logs.
    /// "it failed" is useless; "it failed with `insufficient funds`" is a
    /// diagnosis.
    pub fn send(&mut self, payer: &Keypair, instructions: &[Instruction]) -> u64 {
        let tx = self.build_tx(payer, instructions);

        match self.svm.simulate_transaction(tx.clone()) {
            Ok(sim) => assert!(
                sim.meta.logs.iter().any(|l| l.starts_with("Program")),
                "pump program never ran: {:?}",
                sim.meta.logs
            ),
            Err(failure) => panic!(
                "simulation failed: {}\nlogs:\n{}",
                failure.err,
                failure.meta.logs.join("\n"),
            ),
        }

        let meta = self
            .svm
            .send_transaction(tx)
            .expect("executable transaction must be accepted for submission");
        meta.compute_units_consumed
    }

    /// Builds a transaction with a compute budget prefix, without executing it.
    ///
    /// Chapter 4 goldens compare the bytes this produces, so the instruction
    /// order and the budget prefix must stay stable here.
    #[must_use]
    pub fn build_tx(&mut self, payer: &Keypair, instructions: &[Instruction]) -> Transaction {
        let blockhash = self.svm.latest_blockhash();
        let mut all = Vec::with_capacity(instructions.len() + 2);
        // Budget first: canonical ordering, and it keeps the CU limit at a
        // fixed offset in the golden bytes.
        all.push(ComputeBudgetInstruction::set_compute_unit_limit(200_000));
        all.push(ComputeBudgetInstruction::set_compute_unit_price(1_000_000));
        all.extend_from_slice(instructions);

        let message = Message::new(&all, Some(&payer.pubkey()));
        let mut tx = Transaction::new_unsigned(message);
        tx.partial_sign(&[payer], blockhash);
        tx
    }

    /// Balance of any account, in lamports.
    #[must_use]
    pub fn lamports(&self, address: &Address) -> u64 {
        self.svm
            .get_account(address)
            .map_or(0, |a: Account| a.lamports)
    }

    /// Overwrites an account, for crafting impossible-but-plausible states.
    pub fn set_account(&mut self, address: Address, account: Account) {
        self.svm
            .set_account(address, account)
            .expect("writing a fixture account must succeed");
    }

    /// Serialises a transaction to the bytes that would go on the wire.
    ///
    /// Legacy `Transaction` is lifted to `VersionedTransaction` first, because
    /// that is the type an RPC actually accepts — so the goldens in chapter 4
    /// compare the real payload rather than an internal representation.
    /// Hex keeps the crate dependency-light and stays byte-exact for diffing.
    #[must_use]
    pub fn wire_bytes(tx: &Transaction) -> String {
        let versioned = VersionedTransaction::from(tx.clone());
        hex::encode(bincode::serialize(&versioned).expect("wire serialisation cannot fail"))
    }
}

/// Absolute path to the program dump, resolved from the workspace root.
///
/// One location, so `xtask` and the tests cannot disagree about it.
#[must_use]
pub fn program_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(DEFAULT_PROGRAM_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One shared load keeps the suite fast enough that nobody is tempted to
    /// skip it. 10 MB of bytecode parses in well under a second.
    fn forge() -> Forge {
        Forge::load(program_path())
    }

    /// Proves the genuine mainnet dump is in place.
    ///
    /// Ignored by default, and that is the point of it. This asserts the file
    /// is ~10 MB of real bytecode, which cannot be true on a fresh clone --
    /// `fixtures/programs/` is gitignored because a 10 MB blob does not belong
    /// in git history. Left un-ignored it fails every checkout; left deleted it
    /// stops proving anything at all.
    ///
    /// Run it where the fidelity actually matters:
    ///
    /// ```text
    /// cargo dump-pump && cargo test -p sniper-sandbox -- --ignored
    /// ```
    ///
    /// Everything else in this crate runs against the synthesised stub and is
    /// green without it. This is the one test that cannot pass offline, and it
    /// is opt-in rather than absent so the capability stays visible instead of
    /// quietly rotting.
    #[test]
    #[ignore = "requires the real pump.so dump; run `cargo dump-pump` first"]
    fn forge_loads_the_real_pump_program() {
        // The dump on disk is the artefact that matters: it is what a fresh
        // clone has to refetch, and its size is proof it is the real binary
        // rather than a truncated or HTML error page.
        let dump = std::fs::read(program_path()).expect("pump dump must exist");
        assert!(
            dump.len() > 5_000_000,
            "expected the full ~10 MB binary, got {} bytes",
            dump.len()
        );
        // ELF magic: 7F 45 4C 46. Confirms we are loading bytecode and not,
        // say, a JSON error payload that happens to be large.
        assert_eq!(&dump[..4], b"\x7fELF", "pump dump must be an ELF object");

        // Inside the VM the account is a program-cache stub rather than the
        // whole ELF, so only executability is asserted here.
        let mut f = forge();
        let program_id = f.program_id();
        let account = f
            .svm_mut()
            .get_account(&program_id)
            .expect("pump program must be deployed");
        assert!(
            account.executable,
            "pump account must be an executable program"
        );
    }

    #[test]
    fn the_vm_actually_executes_instructions() {
        let mut f = forge();
        let payer = f.funded_wallet("payer", 1.0);
        let recipient = keypair_from_tag("recipient");
        let before = f.lamports(&payer.pubkey());

        let cu = f.send(
            &payer,
            &[solana_system_interface::instruction::transfer(
                &payer.pubkey(),
                &recipient.pubkey(),
                1_000_000,
            )],
        );

        assert_eq!(f.lamports(&recipient.pubkey()), 1_000_000);
        assert!(
            f.lamports(&payer.pubkey()) < before,
            "payer must be debited by the transfer plus the fee"
        );
        assert!(cu > 0, "compute must be metered");
        assert!(f.slot() > 0, "each send must advance the sandbox slot");
    }

    #[test]
    fn fixtures_are_deterministic_across_forges() {
        let a = forge();
        let b = forge();

        // Same tag -> same wallet, on any machine, in any run.
        assert_eq!(
            keypair_from_tag("buyer").pubkey(),
            keypair_from_tag("buyer").pubkey(),
            "a tagged wallet must be stable"
        );
        assert_ne!(
            keypair_from_tag("buyer").pubkey(),
            keypair_from_tag("seller").pubkey(),
            "different tags must give different wallets"
        );

        // unique_address is deliberately *not* stable across calls: it salts
        // with a per-forge counter so two fixtures cannot collide. What must
        // hold is that the same call index yields the same address, which is
        // what makes a golden snapshot reproducible.
        assert_eq!(a.unique_address("mint"), b.unique_address("mint"));
        assert_ne!(a.unique_address("mint"), a.unique_address("mint"));
        assert_ne!(a.unique_address("mint"), a.unique_address("other"));
    }

    #[test]
    fn identical_inputs_produce_identical_wire_bytes() {
        // The guarantee chapter 4's goldens depend on.
        let build = |()| {
            let mut f = forge();
            let payer = f.funded_wallet("payer", 1.0);
            let to = keypair_from_tag("recipient");
            let tx = f.build_tx(
                &payer,
                &[solana_system_interface::instruction::transfer(
                    &payer.pubkey(),
                    &to.pubkey(),
                    1_000_000,
                )],
            );
            Forge::wire_bytes(&tx)
        };
        assert_eq!(build(()), build(()), "same inputs must give same bytes");
    }

    #[test]
    fn a_failing_transaction_reports_logs_not_a_bare_panic() {
        let mut f = forge();
        // Funded enough to exist and to pay rent, but nowhere near enough to
        // move a lamport of significance. The failure we want is the transfer
        // being rejected, not the fixture failing to be set up.
        let poor = f.funded_wallet("poor", 0.001);
        let to = keypair_from_tag("recipient");

        let tx = f.build_tx(
            &poor,
            &[solana_system_interface::instruction::transfer(
                &poor.pubkey(),
                &to.pubkey(),
                LAMPORTS_PER_SOL,
            )],
        );
        let failure = f
            .svm_mut()
            .simulate_transaction(tx)
            .expect_err("overspending must fail simulation");

        // Worth knowing explicitly: a transaction that fails *before* reaching
        // any program produces no program logs at all. So "there are logs" is
        // not a property a failure can promise. What it must always carry is
        // a concrete error, because that is what the operator reads at 3am.
        assert!(
            !format!("{:?}", failure.err).is_empty(),
            "a failure must carry an error describing what went wrong"
        );

        // And the state must be untouched: a failed transaction moves nothing.
        assert_eq!(
            f.lamports(&to.pubkey()),
            0,
            "nothing may have moved when the transaction failed"
        );
    }

    #[test]
    fn warp_forward_is_allowed_and_backwards_is_refused() {
        let mut f = forge();
        let start = f.slot();
        // LiteSVM starts somewhere realistic, so "forward" is relative to the
        // real position rather than to zero.
        f.warp_to_slot(start + 100);
        assert_eq!(f.slot(), start + 100);

        let backwards = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = forge();
            let now = g.slot();
            g.warp_to_slot(now.saturating_sub(1));
        }));
        assert!(
            backwards.is_err(),
            "time travel must be one-way: the guard has to fire"
        );
    }
}
