<p align="center">
  <img src="logo.png" alt="Scrip" width="160">
</p>

<h1 align="center">Scrip miner</h1>

<p align="center">A reference miner for Scrip, a token issued only by work. CPU by default, OpenCL GPU when built with the `gpu` feature.</p>

---

## What it is

Scrip has no premine, no sale, and no issuance authority. The only way new tokens
exist is a hash below a difficulty threshold, computed against a challenge that
changes every UTC day. You grind, you submit the proof, the chain mints.

This repo is the miner. Contract address:

```
0x50F15a52d251Dfa33862eEe0b5BbB80f5283e9E9
```

## Install

Pick whichever fits. All of them ship the same binary.

```bash
# Rust toolchain
cargo install scrip-miner

# macOS or Linux, via Homebrew
brew install ohmictech/scrip/scrip-miner

# Docker
docker run -it -v scrip:/data ghcr.io/ohmictech/scrip-miner
```

Or download a prebuilt binary for Windows, Linux, or macOS from
[releases](https://github.com/ohmictech/scrip/releases/latest). The release also
ships `SHA256SUMS.txt`; check it before running anything.

To build from source instead: `cargo build --release`.

## Run it

```bash
scrip-miner
```

First run creates `config.toml` and `wallet.json` in the working directory and
prints an address. That address pays gas. Fund it with a little ETH, then run it
again and it mines.

The contract address is pre-filled with the canonical deployment above, and a
blank or missing `contract` falls back to it, so there is nothing to edit.
Everything else has a sane default too: all cores, auto difficulty, public RPC,
gas capped at 5 gwei. `--help` for the flags.

Nothing is submitted until you find a proof, so the only cost of letting it idle
is electricity.

For Docker, `-v scrip:/data` keeps the wallet and config across container
replacements. Without it, a new container generates a new wallet and abandons the
old one's balance.

## GPU mining

The OpenCL backend is behind a build feature, so a plain `cargo build` stays free
of any GPU dependency. Release downloads already include it.

```bash
# build with the GPU backend
cargo build --release --features gpu

# what would be used, if anything
scrip-miner --devices

# verify the kernel against the CPU before trusting it with an epoch
scrip-miner --gpu-selftest

# mine on the GPU
scrip-miner --device gpu
```

Or set it once in `config.toml`:

```toml
device = "auto"   # auto | cpu | gpu | both
```

**The default is `auto`, and it measures rather than guesses:** it runs the CPU
for three seconds and the GPU for three seconds, then keeps whichever is faster
on your machine. That matters because "GPU" does not always mean "faster" — a
weak integrated chip can lose to a many-core CPU, and the reverse is common.

```text
choosing device (3s per engine)...
           cpu 12.70 MH/s
           gpu 27.96 MH/s   Intel(R) UHD Graphics (8192 work-items)
device     gpu Intel(R) UHD Graphics (8192 work-items)   (auto)
```

The two engines are sampled the same way, but a three-second sample is not exact.
If they land within roughly 15% of each other, either choice is fine and the
difference is not worth chasing. Pin one explicitly if you want determinism.

`both` runs the CPU threads and the GPU at the same time. They occupy disjoint
nonce ranges (the GPU owns a reserved stream in the top 16 bits of the counter),
so nothing is ground twice, and both feed one submitter. On this project's test
machine that was additive: 9 MH/s CPU only, 31 MH/s GPU only, 42 MH/s for both.

Asking for `gpu` or `both` when unavailable is an error rather than a quiet fall
back, because silently mining on the CPU at a fraction of the rate would be worse
than a clear failure. Only `auto` falls back.

Notes:

- **Vendor support.** OpenCL is an open standard and all three desktop vendors
  ship a runtime with their normal driver, so the same binary works on Intel,
  NVIDIA, and AMD. `scrip-miner --devices` lists every platform and device found,
  which is the quickest way to tell a missing runtime (no platforms) from an
  unsupported one.
  - **Intel** — verified on this project's hardware (integrated UHD, OpenCL 3.0
    from the graphics driver).
  - **AMD** — expected to work and expected to be reasonably fast: AMD GPUs do
    64-bit integer work at close to full rate, which is what Keccak needs. Ships
    as `amdocl64.dll` on Windows and via `amdgpu-install`/ROCm on Linux, or
    Mesa's Rusticl (`mesa-opencl-icd`).
  - **NVIDIA** — expected to work but likely to underperform relative to its CUDA
    peak. GeForce parts run 64-bit integer operations far below 32-bit rate, and
    this kernel is written with 64-bit lanes because that is the straightforward
    encoding. A 32-bit-lane Keccak would be needed to get close to CUDA-class
    numbers. Ships as `nvopencl64.dll` with the driver.
  - **macOS** — OpenCL is deprecated by Apple and stuck at 1.2. It still works,
    but it is not a path worth depending on; treat the macOS builds as CPU only.
  - **OpenCL CPU devices** (e.g. Mesa/Clover, POCL on Linux) are listed but
    ignored, since the built-in CPU grinders beat them and need no runtime.
- **Any vendor works.** If there is no OpenCL runtime at all, a `gpu` build still
  starts and mines on the CPU.
- **`threads` is ignored on the GPU.** The grid is sized from the device's compute
  units. `threads` still sets how many CPU threads run, which is what `both` adds
  on top of the GPU.
- **Multiple GPUs run in parallel.** Every OpenCL GPU on the machine gets its own
  worker thread, its own context, and its own reserved nonce stream, and they all
  feed one submitter. Up to 256 GPUs (the stream-id budget). If one device fails
  to initialise, it logs and stops while the others keep mining.
- **Hits are re-checked on the CPU** before submission, so a buggy or lying device
  cannot make the miner submit an invalid proof. It can only waste work.
- **Auto-floor measures the selected backend**, so a GPU gets a floor suited to
  its own hashrate rather than the CPU's.

Throughput is what you would expect from the work: a single-block Keccak per
attempt, one permutation each. As a reference point, an integrated Intel UHD in
this project's own testing reached about 31 MH/s against 9 MH/s on the same
machine's CPU. A discrete GPU should be substantially faster than both, and ASICs
faster again; nothing here resists that, by design (`floor` is yours to raise).

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
| high performance ASIC | ~60 TH/s | 61 | 2.0 |

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
| `src/gpu.rs` | OpenCL backend: device setup, launches, hit verification |
| `src/keccak_grind.cl` | the OpenCL kernel, one Keccak permutation per attempt |
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

## License

MIT. See [LICENSE](LICENSE).
