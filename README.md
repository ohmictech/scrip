<p align="center">
  <img src="logo.png" alt="Scrip" width="160">
</p>

<h1 align="center">Scrip miner</h1>

<p align="center">A reference CPU miner for Scrip, a token issued only by proof of work.</p>

---

## What it is

Scrip has no premine, no sale, and no issuance authority. The only way new tokens
exist is a hash below a difficulty threshold, computed against a challenge that
changes every UTC day. You grind, you submit the proof, the chain mints.

This repo is the miner. Contract address:

```
0x50F15a52d251Dfa33862eEe0b5BbB80f5283e9E9
```

## Run it

```bash
cargo run --release
```

First run creates `config.toml` and `wallet.json` and prints an address. That
address pays gas. Fund it with a little ETH, then put the contract address in
`config.toml` and run again:

```toml
contract = "0x50F15a52d251Dfa33862eEe0b5BbB80f5283e9E9"
```

```bash
cargo run --release
```

Everything else has a sane default: all cores, auto difficulty, public RPC, gas
capped at 5 gwei. `--help` for the flags.

Nothing is submitted until you find a proof, so the only cost of letting it idle
is electricity.

## The one number that matters: `floor`

A proof is valid if its hash has at least `floor` leading zero bits. Tokens paid
are `2^(zeros - 40)`, so beating the floor by 10 bits pays 1024×.

`floor` is chosen by *you*, not by the chain. There is no retarget. The default
auto mode measures your machine for three seconds and picks the floor that yields
about `target_proofs_per_day` proofs:

```text
proofs/day = hashrate × 86,400 / 2^floor
```

| Machine | Hashrate | Floor | Proofs/day |
| --- | --- | --- | --- |
| 8-core CPU | ~10 MH/s | 40 | 0.8 |
| 16-core CPU | ~24 MH/s | 41 | 1.0 |
| consumer GPU | ~1 GH/s | 45 | 2.5 |
| ASIC | ~60 TH/s | 61 | 2.0 |

Lower floor = more, smaller notes. Higher floor = fewer, bigger ones.

## Economics

**Every rarity level pays the same in expectation, and the ladder is summed.**
A hash with exactly `k` leading zeros appears with probability `2^-(k+1)` and
pays `2^(k-40)`, so each level contributes `2^-41`. Summing over every level up
to the cap gives the whole emission schedule:

```text
tokens per hash = (129 - 40) / 2 · 2^-40  =  44.5 · 2^-40
```

That means:

- **`floor` does not change your income.** It converts deferred value into more,
  smaller notes. Under-declaring to `floor = 40` earns the same tokens as
  `floor = 47`, but pays 44× more transactions to get them. So the floor
  self-regulates upward to wherever gas stops mattering. No rule is required,
  because the miner pays its own gas.
- **Requiring exact equality would be a 2× tax.** `P(z = k)` is exactly half of
  `P(z ≥ k)`, so a "hit the target exactly" rule throws away half your work.
  A floor keeps the jackpot instead: a lucky hash pays what it is worth.
- **Difficulty never adjusts.** Bitcoin transmits a competitor's entry into your
  income within one difficulty epoch, roughly two weeks. Scrip cannot: emission
  per hash is fixed in the bytecode forever. A machine 100× faster mints 100×
  more, permanently, until price moves.
- **So the arms race is indexed to price.** The early miner's surplus decays on a
  timescale of `float ÷ daily emission`: days while supply is small, months once
  it is large. Being early is worth more here than in Bitcoin, and the rational
  response is to maximize efficiency and reinvest before the float grows.
- **Where it converges.** Once ASICs exist, production cost tracks electricity
  plus fees plus hardware, an industrial commodity price. Dedicated silicon
  dropping the cost of production is the intended endgame, not a failure mode.
- **The cap is load-bearing.** Level contributions are equal, so an uncapped
  ladder would emit infinitely per hash. The 128-bit cap is what makes the supply
  curve converge.

Read that as: emission is a constant of the bytecode, and price is the only
variable that responds. Everyone mining is paid in the same units, proportional
to work, with no schedule to lobby.

## Layout

| File | |
| --- | --- |
| `src/main.rs` | startup, auto-floor, hashrate measurement, submit loop |
| `src/grind.rs` | multithreaded keccak search over nonce ranges |
| `src/tx.rs` | proof encoding, EIP-1559 transaction, RLP |
| `src/rpc.rs` | JSON-RPC over `ureq` |
| `src/wallet.rs` | key generation and loading |
| `src/config.rs` | `config.toml` |

## Warnings

- `wallet.json` holds a private key in plaintext. Keep the balance low, and never
  commit it. `.gitignore` covers it.
- Restarting mid-day is safe: each run starts at a random nonce offset so it
  cannot re-grind proofs whose nullifiers are already spent.
- Proofs are only valid during the UTC day they were ground for. The miner rotates
  automatically at the boundary.
- Gas above `gas_ceiling_gwei` makes it wait for the next block rather than
  overpay.
