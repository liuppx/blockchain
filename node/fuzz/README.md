# Coverage-guided fuzzing (`cargo-fuzz` / libFuzzer)

Deep, nightly fuzzing of the **highest-risk attack surface**: the hand-written wire/snapshot
decoders in [`../src/codec.rs`](../src/codec.rs), fed attacker-controlled bytes off the
network and disk (SECURITY.md §5.1). This complements the stable, always-on
`decoders_reject_adversarial_bytes_without_panicking` test (a deterministic-PRNG "fuzz-lite"
that runs in normal CI) — same invariant, far stronger search.

**Invariant:** every public `codec::decode_*` entry only ever returns `Ok`/`Err` — it must
never panic (OOB index, `unwrap`, debug overflow) and never over-allocate on a forged
length prefix (bounded by `MAX_ITEMS` / `MAX_FRAME`). A libFuzzer crash or OOM is a real bug.

## Requirements

```sh
rustup toolchain install nightly        # libFuzzer needs nightly rustc
cargo install cargo-fuzz                 # the `cargo fuzz` subcommand
```

## Run

From the `node/` crate directory:

```sh
cargo +nightly fuzz build decode_all               # compile the instrumented target
cargo +nightly fuzz run decode_all                 # fuzz forever (Ctrl-C to stop)
cargo +nightly fuzz run decode_all -- -max_total_time=300   # time-boxed (CI uses this)
cargo +nightly fuzz list                           # show targets
```

New interesting inputs are written into [`corpus/decode_all/`](corpus/decode_all), which is
**committed** so each run starts warm and CI exercises a known seed set. Commit any corpus
growth that meaningfully improves coverage; prune with `cargo +nightly fuzz cmin decode_all`.

## Target shape

`decode_all` is **multiplexed**: input byte 0 selects a decoder (`sel % 30`), the rest is the
frame. One corpus hammers all 30 parsers and mutations cross-pollinate between them. Adding a
new `decode_*` is a one-line arm in [`fuzz_targets/decode_all.rs`](fuzz_targets/decode_all.rs)
(the modulus auto-grows).

## Triage

A crash drops a reproducer in `artifacts/decode_all/`. Re-run it and get a backtrace:

```sh
RUST_BACKTRACE=1 cargo +nightly fuzz run decode_all artifacts/decode_all/crash-<hash>
```

Then add the offending bytes as a regression to the stable fuzz-lite test (so the fix is
guarded on stable CI, not just nightly).
