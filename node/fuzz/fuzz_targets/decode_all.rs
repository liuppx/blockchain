#![no_main]
//! M213: coverage-guided (libFuzzer) fuzzing of every public `codec::decode_*` entry.
//!
//! The wire/snapshot decoders are the highest-risk attack surface (hand-written parsers
//! fed attacker-controlled bytes off the network/disk — SECURITY.md §5.1). This is the
//! deep, nightly counterpart to the stable `decoders_reject_adversarial_bytes_without_
//! panicking` test (which hammers them with a deterministic PRNG): same invariant,
//! stronger search.
//!
//! **Invariant under test:** every entry only ever returns `Ok`/`Err` — never panics
//! (index out of bounds, unwrap, arithmetic overflow in debug, …) and never over-allocates
//! on a forged length prefix (guarded by `MAX_ITEMS`/`MAX_FRAME`). A libFuzzer crash or OOM
//! is a real finding.
//!
//! Multiplexed target: the first input byte selects a decoder, the rest is the frame, so a
//! single corpus hammers all parsers and mutations cross-pollinate. Run:
//!   (cd node && cargo +nightly fuzz run decode_all)
//! See `node/fuzz/README.md`.

use libfuzzer_sys::fuzz_target;
use zhixing_node::codec;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, b)) = data.split_first() else {
        return;
    };
    // Keep this list in sync with `node/fuzz/README.md` and the stable fuzz-lite test.
    // The `% N` dispatch means new decoders only need an arm appended (N auto-grows).
    match sel % 29 {
        0 => {
            let _ = codec::decode_header(b);
        }
        1 => {
            let _ = codec::decode_certified_header(b);
        }
        2 => {
            let _ = codec::decode_header_any(b);
        }
        3 => {
            let _ = codec::decode_validator_set(b);
        }
        4 => {
            let _ = codec::decode_tx(b);
        }
        5 => {
            let _ = codec::decode_stakeop(b);
        }
        6 => {
            let _ = codec::decode_key_rotation(b);
        }
        7 => {
            let _ = codec::decode_gov_proposal(b);
        }
        8 => {
            let _ = codec::decode_gov_vote(b);
        }
        9 => {
            let _ = codec::decode_bridge_lock(b);
        }
        10 => {
            let _ = codec::decode_bridge_header(b);
        }
        11 => {
            let _ = codec::decode_bridge_redeem(b);
        }
        12 => {
            let _ = codec::decode_bridge_unbond(b);
        }
        13 => {
            let _ = codec::decode_consensus_msg(b);
        }
        14 => {
            let _ = codec::decode_evidence(b);
        }
        15 => {
            let _ = codec::decode_unbonding(b);
        }
        16 => {
            let _ = codec::decode_commit(b);
        }
        17 => {
            let _ = codec::decode_account(b);
        }
        18 => {
            let _ = codec::decode_snapshot(b);
        }
        19 => {
            let _ = codec::decode_proof(b);
        }
        // Single-byte discriminant decoders: feed the next byte (or 0 when absent).
        20 => {
            let _ = codec::decode_proof_kind(b.first().copied().unwrap_or(0));
        }
        21 => {
            let _ = codec::decode_batch_response_kind(b.first().copied().unwrap_or(0));
        }
        22 => {
            let _ = codec::decode_knn_request(b);
        }
        23 => {
            let _ = codec::decode_range_request(b);
        }
        24 => {
            let _ = codec::decode_validator(b);
        }
        25 => {
            let _ = codec::decode_reviewer(b);
        }
        26 => {
            let _ = codec::decode_graph_node(b);
        }
        27 => {
            let _ = codec::decode_proof_entry(b);
        }
        28 => {
            let _ = codec::decode_block(b);
        }
        _ => unreachable!("sel % 29 is always 0..=28"),
    }
});
