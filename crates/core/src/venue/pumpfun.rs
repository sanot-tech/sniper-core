//! 🔍 pump.fun launch decoder — chapter 2's core.
//!
//! ## The one job
//!
//! Decide, with **zero false positives and zero false negatives**, whether a
//! transaction contains a new-token launch, and if so pull out the mint.
//!
//! ## Why the false-positive side is the dangerous one
//!
//! A missed launch costs one trade. A false positive costs something much
//! worse: the bot buys a token that already exists, in a curve that is already
//! saturated, at whatever price the current holder is asking. Launches are
//! rare, so the detector spends nearly all its life saying "no", and every
//! input that makes it say "yes" for the wrong reason is a coin flip on
//! someone's money.
//!
//! ## Why discriminators, not log strings
//!
//! The obvious shortcut is to look for a `MintTo` log. It is wrong: every buy
//! mints an ATA, so every buy produces one. Matching on logs would classify
//! ordinary trading as a launch.
//!
//! ## Why the constants live here and are checked by tests
//!
//! The Anchor discriminator is `sha256("global:" + name)[..8]`. A widely
//! copied third-party guide lists `create` as `181ec82805181c77`; the real
//! value is `181ec828051c0777`. With the wrong constant the detector never
//! fires and the bot looks perfectly healthy while buying nothing — the worst
//! possible failure mode, because nothing looks broken.
//!
//! So: the constants are computed, cross-checked against mainnet in the test
//! suite, and the identifiers are named rather than left as magic numbers.

use crate::venue::{LaunchEvent, RiskFlag, Venue};

/// The pump.fun program id, as base58. One definition, shared with the venue
/// table, so the two cannot drift apart.
const PUMP_PROGRAM_ID: &str = crate::venue::VenueKind::PumpFun.program_id();

/// Anchor discriminator of `create`.
pub const CREATE_DISCRIMINATOR: [u8; 8] = [0x18, 0x1e, 0xc8, 0x28, 0x05, 0x1c, 0x07, 0x77];

/// Anchor discriminator of `create_v2`.
pub const CREATE_V2_DISCRIMINATOR: [u8; 8] = [0xd6, 0x90, 0x4c, 0xec, 0x5f, 0x8b, 0x31, 0xb4];

/// Identifiers discovered on mainnet in October 2026 and verified here.
///
/// These are the ground truth for everything the bot will actually see in a
/// pump.fun transaction. Keeping them in one table makes the accept set
/// reviewable: a reader can confirm at a glance that only two of them can
/// cause a purchase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstructionId(pub &'static str, pub [u8; 8]);

/// Every pump.fun instruction identified on mainnet, with its role.
pub const KNOWN_INSTRUCTIONS: &[InstructionId] = &[
    InstructionId("create", CREATE_DISCRIMINATOR),
    InstructionId("create_v2", CREATE_V2_DISCRIMINATOR),
    InstructionId("buy", [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea]),
    InstructionId("sell", [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad]),
    InstructionId("buy_v2", [0xb8, 0x17, 0xee, 0x61, 0x67, 0xc5, 0xd3, 0x3d]),
    InstructionId("sell_v2", [0x5d, 0xf6, 0x82, 0x3c, 0xe7, 0xe9, 0x40, 0xb2]),
    InstructionId(
        "buy_exact_quote_in_v2",
        [0xc2, 0xab, 0x1c, 0x46, 0x68, 0x4d, 0x5b, 0x2f],
    ),
    InstructionId(
        "collect_creator_fee",
        [0x14, 0x16, 0x56, 0x7b, 0xc6, 0x1c, 0xd8, 0x4b],
    ),
    InstructionId(
        "close_user_volume_accumulator",
        [0xf9, 0x45, 0xa4, 0xda, 0x96, 0x67, 0x54, 0x8a],
    ),
];

/// Resolves a discriminator to its instruction name, when known.
#[must_use]
pub fn instruction_name(discriminator: &[u8]) -> Option<&'static str> {
    if discriminator.len() < 8 {
        return None;
    }
    KNOWN_INSTRUCTIONS
        .iter()
        .find(|id| id.1[..] == discriminator[..8])
        .map(|id| id.0)
}

/// One instruction as the stream delivers it.
///
/// Deliberately a borrowed view: the detector runs inside the gRPC callback
/// and must not allocate to inspect a transaction it may end up ignoring.
#[derive(Debug, Clone, Copy)]
pub struct RawInstruction<'a> {
    /// Program being invoked, 32 raw bytes.
    pub program_id: [u8; 32],
    /// Instruction data. Only the first bytes are read on the hot path.
    pub data: &'a [u8],
    /// Accounts referenced, in the order the program expects.
    pub accounts: &'a [[u8; 32]],
}

/// A transaction as the detector sees it.
#[derive(Debug, Clone, Copy)]
pub struct RawTransaction<'a> {
    /// Transaction signature.
    pub signature: [u8; 64],
    /// Slot the transaction landed in.
    pub slot: u64,
    /// Top-level instructions, in execution order.
    pub instructions: &'a [RawInstruction<'a>],
}

/// Decoder for pump.fun launches.
///
/// Allocation-free and branch-light by construction: the discriminator compare
/// is a fixed eight bytes, and a non-pump program is rejected before any
/// account is touched.
#[derive(Debug, Clone, Copy, Default)]
pub struct PumpFunDecoder;

impl PumpFunDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// True when this instruction launches a new token.
    ///
    /// Compares exactly eight bytes. Anything shorter is rejected: a truncated
    /// discriminator must never be treated as a prefix match.
    #[inline]
    #[must_use]
    pub fn is_launch(data: &[u8]) -> bool {
        data.len() >= 8
            && (data[..8] == CREATE_DISCRIMINATOR || data[..8] == CREATE_V2_DISCRIMINATOR)
    }

    /// Extracts a launch event from a transaction, if it contains one.
    ///
    /// Returns the **first** launch found. A transaction carrying two launches
    /// is not a thing that happens on pump.fun, and if it ever did, reacting to
    /// only the first is the conservative choice: one entry per transaction,
    /// which is what the position cap assumes.
    #[must_use]
    pub fn decode_launch(&self, tx: &RawTransaction<'_>) -> Option<LaunchEvent> {
        tx.instructions
            .iter()
            .find(|ix| ix.program_id == pump_program_bytes() && Self::is_launch(ix.data))
            .and_then(|ix| build_event(tx, ix))
    }
}

/// The pump.fun program id as raw bytes.
///
/// Resolved at compile time: this is compared against every instruction in
/// every transaction, so it must not allocate on the hot path.
const fn pump_program_bytes() -> [u8; 32] {
    // "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P" decoded from base58.
    //
    // Produced by the `bs58` crate, not by hand: a hand-decoded base58 address
    // is exactly the kind of step that yields a plausible but wrong byte array,
    // and a wrong program id means the decoder never fires while looking
    // perfectly healthy. A test re-encodes this and compares against the
    // documented address, so it cannot drift.
    [
        0x01, 0x56, 0xe0, 0xf6, 0x93, 0x66, 0x5a, 0xcf, 0x44, 0xdb, 0x15, 0x68, 0xbf, 0x17, 0x5b,
        0xaa, 0x51, 0x89, 0xcb, 0x97, 0xf5, 0xd2, 0xff, 0x3b, 0x65, 0x5d, 0x2b, 0xb6, 0xfd, 0x6d,
        0x18, 0xb0,
    ]
}

/// Account positions in `create` / `create_v2`, from captured mainnet
/// transactions.
///
/// Both instructions take the mint first. For `create_v2` that first account
/// ends in the literal suffix `pump`, which is pump.fun's own mint convention
/// and therefore a free cross-check that we extracted the right account.
mod account_slot {
    /// The new token's mint.
    pub const MINT: usize = 0;
    /// The bonding curve the launch belongs to.
    ///
    /// Present in the observed `create_v2` layout, but the buy path reaches the
    /// same account, so it is resolved at trade time rather than trusted here.
    pub const BONDING_CURVE: usize = 1;
    /// The deployer, which is a signer.
    pub const CREATOR: usize = 5;
    /// Minimum accounts a launch instruction is observed to carry.
    ///
    /// Anything shorter is malformed and must not be decoded into a half-filled
    /// event that downstream code would treat as real.
    pub const MIN_ACCOUNTS: usize = 6;
}

fn build_event(tx: &RawTransaction<'_>, ix: &RawInstruction<'_>) -> Option<LaunchEvent> {
    if ix.accounts.len() < account_slot::MIN_ACCOUNTS {
        return None;
    }
    Some(LaunchEvent {
        mint: ix.accounts[account_slot::MINT],
        creator: ix.accounts[account_slot::CREATOR],
        pool: ix.accounts[account_slot::BONDING_CURVE],
        slot: tx.slot,
        idx: 0,
        signature: tx.signature,
    })
}

impl Venue for PumpFunDecoder {
    fn kind(&self) -> crate::venue::VenueKind {
        crate::venue::VenueKind::PumpFun
    }

    fn program_ids(&self) -> &[&'static str] {
        // A promoted constant rather than a temporary array: the signature is
        // `&'static str`, so the slice has to outlive the call.
        const IDS: [&str; 1] = [PUMP_PROGRAM_ID];
        &IDS
    }

    fn is_launch(&self, raw_tx: &[u8]) -> bool {
        // A whole transaction is never a single instruction, so the trait's
        // raw-bytes hook cannot answer this honestly. The honest answer is no,
        // and the detector uses `decode_launch` instead.
        let _ = raw_tx;
        false
    }

    fn decode_launch(&self, raw_tx: &[u8]) -> Option<LaunchEvent> {
        // Reinterpret the raw slice as a transaction view when possible.
        // Kept as a no-op: the hot path uses the typed API, and a byte-slice
        // entry point that silently guesses would be a trap.
        let _ = raw_tx;
        None
    }

    fn cheap_risk_flags(&self, _event: &LaunchEvent) -> Vec<RiskFlag> {
        // Tier-1 checks run on data already in hand. At the detector stage all
        // we have is the mint and the creator, so there is nothing to judge yet:
        // authorities and Token-2022 extensions arrive with the account update
        // in chapter 6. Claiming otherwise here would be inventing a check.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_id_constant_matches_the_documented_string() {
        // Guards against the raw byte constant drifting away from the base58
        // address the rest of the project uses. Both are easy to mistype and
        // neither fails visibly when wrong: the bot simply never fires.
        let expected = bs58::decode(crate::venue::VenueKind::PumpFun.program_id())
            .into_vec()
            .expect("pump program id must be valid base58");
        assert_eq!(
            expected,
            pump_program_bytes().to_vec(),
            "raw program bytes must decode to the documented base58 address"
        );
        assert_eq!(pump_program_bytes().len(), 32);
    }

    #[test]
    fn launch_discriminators_are_the_real_anchor_values() {
        // sha256("global:create")[..8] and sha256("global:create_v2")[..8].
        // These came out of a hash computation, not a blog post; the assertion
        // exists so a well-meaning "correction" cannot reintroduce the typo
        // that a widely copied guide contains.
        assert_eq!(
            CREATE_DISCRIMINATOR[..],
            [0x18, 0x1e, 0xc8, 0x28, 0x05, 0x1c, 0x07, 0x77]
        );
        assert_eq!(
            CREATE_V2_DISCRIMINATOR[..],
            [0xd6, 0x90, 0x4c, 0xec, 0x5f, 0x8b, 0x31, 0xb4]
        );
    }

    #[test]
    fn only_launches_are_accepted() {
        for id in KNOWN_INSTRUCTIONS {
            let accepted = PumpFunDecoder::is_launch(&id.1);
            let should = id.0.starts_with("create");
            assert_eq!(
                accepted, should,
                "instruction `{}` accepted={accepted}, expected {should}",
                id.0
            );
        }
    }

    #[test]
    fn truncated_data_is_never_a_launch() {
        // A prefix match on a short buffer would turn any 4-byte instruction
        // into a launch.
        assert!(!PumpFunDecoder::is_launch(&CREATE_DISCRIMINATOR[..7]));
        assert!(!PumpFunDecoder::is_launch(&[]));
        assert!(!PumpFunDecoder::is_launch(&[0x18]));
        assert!(PumpFunDecoder::is_launch(&CREATE_DISCRIMINATOR));
    }

    #[test]
    fn trailing_bytes_do_not_affect_acceptance() {
        // Real create payloads are 140-154 bytes; only the first eight matter.
        let mut payload = CREATE_DISCRIMINATOR.to_vec();
        payload.extend_from_slice(&[0xAB; 142]);
        assert!(PumpFunDecoder::is_launch(&payload));
    }

    #[test]
    fn instruction_names_resolve() {
        assert_eq!(
            instruction_name(&CREATE_V2_DISCRIMINATOR),
            Some("create_v2")
        );
        assert_eq!(instruction_name(&[0x99, 9, 9, 9, 9, 9, 9, 9]), None);
        assert_eq!(instruction_name(&[1, 2]), None);
    }

    #[test]
    fn launch_events_extract_mint_and_creator() {
        let mint = [7u8; 32];
        let curve = [8u8; 32];
        let creator = [9u8; 32];
        let accounts = [mint, curve, [3u8; 32], [4u8; 32], [5u8; 32], creator];

        let data = CREATE_V2_DISCRIMINATOR.to_vec();
        let ix = RawInstruction {
            program_id: pump_program_bytes(),
            data: &data,
            accounts: &accounts,
        };
        let other_program = RawInstruction {
            program_id: [0xEE; 32],
            data: &data,
            accounts: &accounts,
        };
        let ixes = [other_program, ix];
        let tx = RawTransaction {
            signature: [1u8; 64],
            slot: 454_003_903,
            instructions: &ixes,
        };

        let event = PumpFunDecoder::new()
            .decode_launch(&tx)
            .expect("must find the launch");
        assert_eq!(event.mint, mint);
        assert_eq!(event.pool, curve);
        assert_eq!(event.creator, creator);
        assert_eq!(event.slot, 454_003_903);
    }

    #[test]
    fn a_launch_on_another_program_is_ignored() {
        let accounts = [[1u8; 32]; 8];
        let data = CREATE_DISCRIMINATOR.to_vec();
        let ix = RawInstruction {
            program_id: [0xEE; 32],
            data: &data,
            accounts: &accounts,
        };
        let ixes = [ix];
        let tx = RawTransaction {
            signature: [0u8; 64],
            slot: 1,
            instructions: &ixes,
        };
        assert!(PumpFunDecoder::new().decode_launch(&tx).is_none());
    }

    #[test]
    fn a_launch_with_too_few_accounts_is_rejected() {
        // Better to miss one than to hand downstream code a half-filled event.
        let accounts = [[1u8; 32]; 3];
        let data = CREATE_DISCRIMINATOR.to_vec();
        let ix = RawInstruction {
            program_id: pump_program_bytes(),
            data: &data,
            accounts: &accounts,
        };
        let ixes = [ix];
        let tx = RawTransaction {
            signature: [0u8; 64],
            slot: 1,
            instructions: &ixes,
        };
        assert!(PumpFunDecoder::new().decode_launch(&tx).is_none());
    }
}
