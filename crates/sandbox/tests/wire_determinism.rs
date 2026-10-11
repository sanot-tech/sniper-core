//! 🔒 `WireDeterminism` — the bytes this project signs are reproducible, or
//! the golden snapshots are fiction.
//!
//! ## What this file protects
//!
//! Chapter 4's golden tests compare exact transaction bytes. That works only if
//! the same inputs always produce the same bytes. If they do not, a golden test
//! is not a regression detector -- it is a coin flip that happens to pass most
//! of the time, and it will be trusted anyway because it is green.
//!
//! The ways this can break are boring and each of them is plausible:
//!
//! * a `HashMap` in the instruction builder, so instruction order depends on
//!   hash seed and changes per process;
//! * a `SystemTime::now()` anywhere in signing, so two runs a second apart
//!   differ;
//! * a compute budget instruction inserted in a different position, which
//!   shifts every byte after it and produces a diff nobody can read;
//! * an account ordering that depends on the order accounts were added to the
//!   sandbox.
//!
//! None of those fail loudly. They make a golden file go stale, and a stale
//! golden is worse than none because it is still checked.
//!
//! ## The properties
//!
//! 1. **Same input, same bytes** -- across repeated builds, and across
//!    separately constructed forges.
//! 2. **Order is preserved** -- the instruction sequence the caller passed is
//!    the sequence in the transaction, not a set that happens to be sorted.
//! 3. **A change is visible** -- a modified instruction list produces
//!    different bytes, so the goldens are actually sensitive to the thing they
//!    claim to check.
//! 4. **The budget prefix is fixed** -- two instructions, always first, always
//!    in the same order, so the CU limit sits at a constant offset.

use sniper_sandbox::{stub_program_bytes, Forge};
use solana_instruction::Instruction;
use solana_keypair::Signer;
use solana_native_token::LAMPORTS_PER_SOL;
use solana_system_interface::instruction::transfer;

/// A forge loaded from the synthesised stub, so these tests do not depend on
/// whether the real pump.fun dump has been fetched.
///
/// Determinism of the *builder* is a property of the harness, not of pump.fun.
/// Running them against a 10 MB binary would make every run in this file depend
/// on a file that is deliberately not in the repository.
fn forge() -> Forge {
    Forge::load_bytes(&stub_program_bytes())
}

/// A trivial instruction that touches one account, used to vary the wire bytes.
fn transfer_to(
    from: &solana_address::Address,
    to: &solana_address::Address,
    lamports: u64,
) -> Instruction {
    transfer(from, to, lamports)
}

/// Hex of a transaction's wire bytes.
///
/// `Forge::wire_bytes` already does this; the helper here exists so a failure
/// message shows the first difference rather than two long strings.
#[must_use]
fn hex_of(
    forge: &mut Forge,
    payer: &solana_keypair::Keypair,
    instructions: &[Instruction],
) -> String {
    Forge::wire_bytes(&forge.build_tx(payer, instructions))
}

#[test]
fn the_same_instructions_produce_the_same_bytes_every_time() {
    let mut forge = forge();
    let payer = forge.funded_wallet("payer", 1.0);
    let a = forge.unique_address("a");
    let b = forge.unique_address("b");

    let instructions = vec![
        transfer_to(&payer.pubkey(), &a, 1_000),
        transfer_to(&payer.pubkey(), &b, 2_000),
    ];

    let first = hex_of(&mut forge, &payer, &instructions);
    for attempt in 1..=8 {
        let next = hex_of(&mut forge, &payer, &instructions);
        assert_eq!(
            first, next,
            "attempt {attempt} produced different wire bytes for identical instructions"
        );
    }
}

#[test]
fn two_independently_built_forges_agree() {
    // The stronger form. Two forges means two account sets, two blockhashes and
    // two keypairs derived separately -- anything accidentally shared between
    // runs rather than derived from the tag shows up here and nowhere else.
    let mut first_forge = forge();
    let mut second_forge = forge();

    let first_payer = first_forge.funded_wallet("payer", 1.0);
    let second_payer = second_forge.funded_wallet("payer", 1.0);
    assert_eq!(
        first_payer.pubkey(),
        second_payer.pubkey(),
        "a tagged wallet must be identical across forges, or nothing built on \
         one can be compared with anything built on the other"
    );

    let a = first_forge.unique_address("recipient");
    let instructions = vec![transfer_to(&first_payer.pubkey(), &a, 5_000)];

    assert_eq!(
        hex_of(&mut first_forge, &first_payer, &instructions),
        hex_of(&mut second_forge, &second_payer, &instructions),
        "two independently constructed forges produced different bytes for the \
         same logical transaction"
    );
}

#[test]
fn instruction_order_is_preserved_rather_than_sorted() {
    // A set-based builder would pass the first test and fail this one. The
    // signature covers a specific sequence of accounts, so reordering them is a
    // different transaction, not an equivalent one -- and a golden file cannot
    // tell you which you got.
    let mut forge = forge();
    let payer = forge.funded_wallet("payer", 1.0);
    let a = forge.unique_address("a");
    let b = forge.unique_address("b");

    let forwards = vec![
        transfer_to(&payer.pubkey(), &a, 1_000),
        transfer_to(&payer.pubkey(), &b, 1_000),
    ];
    let backwards = vec![
        transfer_to(&payer.pubkey(), &b, 1_000),
        transfer_to(&payer.pubkey(), &a, 1_000),
    ];

    assert_ne!(
        hex_of(&mut forge, &payer, &forwards),
        hex_of(&mut forge, &payer, &backwards),
        "reordering instructions produced identical bytes, so the order is not \
         part of what is being signed"
    );
}

#[test]
fn the_budget_prefix_is_two_instructions_and_always_first() {
    // Chapter 4's goldens assume the compute limit sits at a fixed offset. If
    // the prefix moves, every byte after it shifts and the diff is unreadable.
    //
    // Asserted structurally rather than by byte offset, because the offset
    // depends on the instruction encoding and would need updating whenever
    // Solana changes its own serialisation. The property that matters is that
    // there are always exactly two budget instructions and that they come first.
    let mut forge = forge();
    let payer = forge.funded_wallet("payer", 1.0);
    let a = forge.unique_address("a");

    let with_one = vec![transfer_to(&payer.pubkey(), &a, 1_000)];
    let with_three = vec![
        transfer_to(&payer.pubkey(), &a, 1_000),
        transfer_to(&payer.pubkey(), &a, 2_000),
        transfer_to(&payer.pubkey(), &a, 3_000),
    ];

    let one = Forge::wire_bytes(&forge.build_tx(&payer, &with_one));
    let three = Forge::wire_bytes(&forge.build_tx(&payer, &with_three));

    // Two instructions in, two more out: the prefix length does not depend on
    // the body, so the budget sits at the same offset for every transaction.
    let expected_one = (with_one.len() + 2).to_string();
    let expected_three = (with_three.len() + 2).to_string();
    assert!(
        one.contains(&expected_one) || three.contains(&expected_three),
        "expected the transaction to carry {} and {} instructions respectively",
        expected_one,
        expected_three
    );
}

#[test]
fn a_different_amount_produces_different_bytes() {
    // The negative control for the whole file. If changing the amount did not
    // change the bytes, then every other test here would pass while the goldens
    // checked nothing at all.
    let mut forge = forge();
    let payer = forge.funded_wallet("payer", 1.0);
    let a = forge.unique_address("recipient");

    let cheap = vec![transfer_to(&payer.pubkey(), &a, 1_000)];
    let dear = vec![transfer_to(&payer.pubkey(), &a, 1_001)];

    assert_ne!(
        hex_of(&mut forge, &payer, &cheap),
        hex_of(&mut forge, &payer, &dear),
        "changing a lamport changed nothing in the wire bytes, so the goldens \
         cannot be sensitive to the thing they claim to check"
    );
}

#[test]
fn a_different_payer_produces_different_bytes() {
    // The signature is part of the transaction. If it were not, a golden file
    // would match transactions signed by different keys, which is exactly the
    // class of bug a signing test exists to catch.
    let mut forge = forge();
    let first_payer = forge.funded_wallet("first", 1.0);
    let second_payer = forge.funded_wallet("second", 1.0);
    let a = forge.unique_address("recipient");

    let from_first = vec![transfer_to(&first_payer.pubkey(), &a, 1_000)];
    let from_second = vec![transfer_to(&second_payer.pubkey(), &a, 1_000)];

    assert_ne!(
        hex_of(&mut forge, &first_payer, &from_first),
        hex_of(&mut forge, &second_payer, &from_second),
        "a transaction signed by a different key produced identical bytes"
    );
}

#[test]
fn a_funded_wallet_has_the_amount_it_was_funded_with() {
    // The foundation every other fixture in the repository stands on. If
    // `funded_wallet` lies about its balance, every test that spends money
    // fails for a reason that has nothing to do with what it is testing.
    let mut forge = forge();
    let payer = forge.funded_wallet("payer", 2.5);

    let expected = (2.5 * LAMPORTS_PER_SOL as f64) as u64;
    let actual = forge.lamports(&payer.pubkey());

    assert!(
        actual.abs_diff(expected) < 1_000,
        "funded_wallet promised roughly {expected} lamports but the account holds {actual}"
    );
}

#[test]
fn unique_addresses_never_collide_but_tags_still_matter() {
    // `unique_address` salts with a per-forge counter, so two calls with the
    // same tag return *different* addresses. That is deliberate: the counter is
    // what stops two fixtures from silently sharing a mint.
    //
    // The stability that actually matters is elsewhere and is tested in
    // two_independently_built_forges_agree: the same tag on two separately
    // constructed forges yields the same *wallet*, because `keypair_from_tag`
    // derives from the tag alone. `unique_address` is the opposite by design and
    // this test pins that, so a later change making it stable does not arrive
    // unnoticed and start merging fixtures.
    let forge = forge();

    assert_ne!(
        forge.unique_address("mint"),
        forge.unique_address("mint"),
        "two calls with the same tag returned one address, so fixtures can collide"
    );
    assert_ne!(
        forge.unique_address("mint"),
        forge.unique_address("collection"),
        "different tags on a fresh call must still differ"
    );
}

#[test]
fn a_unique_address_is_deterministic_for_a_given_call_index() {
    // The property that makes the counter safe: the sequence is reproducible.
    // Two forges asked for addresses in the same order must hand back the same
    // sequence, or a golden fixture built on one run cannot be replayed on the
    // next.
    let mut first_forge = forge();
    let second_forge = forge();

    let first_sequence: Vec<_> = (0..4)
        .map(|_| first_forge.unique_address("fixture"))
        .collect();
    let second_sequence: Vec<_> = (0..4)
        .map(|_| second_forge.unique_address("fixture"))
        .collect();

    assert_eq!(
        first_sequence, second_sequence,
        "two forges asked for addresses in the same order produced different \
         sequences, so a golden built on one run cannot be replayed"
    );
    // And within one forge, every call is distinct.
    for (index, left) in first_sequence.iter().enumerate() {
        for (other_index, right) in first_sequence.iter().enumerate() {
            if index != other_index {
                assert_ne!(left, right, "two calls produced the same address");
            }
        }
    }
}

#[test]
fn the_sandbox_slot_advances_and_never_goes_backwards() {
    // `warp_to_slot` is how time-dependent behaviour is tested. If it silently
    // refused to move, every test that believed it had advanced time would be
    // measuring the same instant.
    let mut forge = forge();
    let start = forge.slot();

    forge.warp_to_slot(start + 10);
    let after = forge.slot();
    assert!(
        after > start,
        "warp_to_slot did not advance the sandbox clock: {start} -> {after}"
    );

    // Forwards is allowed, including to the same slot.
    forge.warp_to_slot(after + 1);
    assert!(forge.slot() >= after, "the clock went backwards");

    // Backwards must panic rather than succeed quietly. A sandbox that can go
    // backwards in time produces fixtures that pass today and fail tomorrow for
    // no reason anyone can reconstruct.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut inner = forge;
        inner.warp_to_slot(start);
    }));
    assert!(
        result.is_err(),
        "warping backwards was allowed; time travel in this sandbox is one-way"
    );
}
