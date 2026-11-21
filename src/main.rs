//! Scrip reference miner.
//!
//! Single command: creates `config.toml` and `wallet.json` beside itself on the
//! first run, then mines continuously, submitting each proof as it is found.

mod config;
mod grind;
mod rpc;
mod tx;
mod wallet;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use k256::ecdsa::SigningKey;
use rpc::Rpc;
use tx::{Submission, TxRequest};

const GWEI: u128 = 1_000_000_000;
const FALLBACK_GAS: u64 = 400_000;

fn main() {
    if let Err(e) = run() {
        eprintln!("\n[error] {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args().skip(1))?;
    if args.help {
        print_help();
        return Ok(());
    }

    let config_path = args.config.clone().unwrap_or_else(|| PathBuf::from("config.toml"));
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    println!("scrip-miner {}", env!("CARGO_PKG_VERSION"));

    // --- config and wallet, created on first run ---------------------------
    let created_config = !config_path.exists();
    if created_config {
        // The template embeds the wallet address, so the wallet comes first.
        let provisional = wallet::load_or_create(&dir.join("wallet.json"))?;
        config::load_or_create(&config_path, &provisional.address_hex())?;
        println!("created {}", config_path.display());
        println!("created {}", dir.join("wallet.json").display());
    }

    let (cfg, _) = config::load_or_create(&config_path, "")?;
    let wallet_path = resolve(&dir, &cfg.wallet);
    let wallet = wallet::load_or_create(&wallet_path)?;

    // --- contract ----------------------------------------------------------
    let contract_hex = args
        .contract
        .clone()
        .unwrap_or_else(|| cfg.contract.clone())
        .trim()
        .to_string();

    if contract_hex.is_empty() {
        println!();
        println!("One thing left: set `contract` in {}", config_path.display());
        println!("Then run this command again and mining starts immediately.");
        println!();
        println!("Wallet address to fund with a little ETH for gas:");
        println!("    {}", wallet.address_hex());
        return Ok(());
    }

    let contract = parse_address(&contract_hex)?;
    let payout = if cfg.payout.trim().is_empty() {
        wallet.address
    } else {
        parse_address(&cfg.payout)?
    };
    if payout == [0u8; 20] {
        return Err("payout is the zero address; every proof would be burned".into());
    }

    // --- chain -------------------------------------------------------------
    let rpc = Rpc::new(&cfg.rpc);
    let chain_id = rpc.chain_id()?;
    let code = rpc.code(&contract_hex)?;
    if code.trim() == "0x" || code.trim().is_empty() {
        return Err(format!("no contract code at {contract_hex} on chain {chain_id}"));
    }

    let salt = rpc::word32(&rpc.eth_call(&contract_hex, &selector("SALT()"))?)
        .ok_or("could not read SALT()")?;
    let base = rpc::parse_hex(&rpc.eth_call(&contract_hex, &selector("BASE_DIFFICULTY()"))?)
        .ok_or("could not read BASE_DIFFICULTY()")? as u8;

    let threads = if cfg.threads == 0 {
        thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    } else {
        cfg.threads
    };

    // Random 48-bit start offset for this run's nonce ranges. Restarting inside
    // an epoch must not re-grind nonces whose nullifiers are already spent.
    let run_start = {
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| format!("entropy: {e}"))?;
        u64::from_be_bytes(seed) & 0x0000_FFFF_FFFF_FFFF
    };

    // `floor = 0` means auto: measure this machine and pick the rarity that lands
    // near `target_proofs_per_day`. Without it the default floor equals
    // BASE_DIFFICULTY, which floods the chain from a GPU and starves a CPU.
    let (floor, measured) = if cfg.floor == 0 {
        println!();
        println!("measuring hashrate (3s)...");
        let h = measure_hashrate(threads, run_start);
        let f = auto_floor(h, base, cfg.target_proofs_per_day);
        println!(
            "           {:.2} MH/s -> floor {} (target {:.1} proofs/day, est {:.2})",
            h / 1e6,
            f,
            cfg.target_proofs_per_day,
            h * 86_400.0 / 2f64.powi(f as i32)
        );
        (f, Some(h))
    } else if cfg.floor < base {
        println!("floor {} is below BASE_DIFFICULTY {base}; using {base}", cfg.floor);
        (base, None)
    } else {
        (cfg.floor, None)
    };

    let balance = rpc.balance(&wallet.address_hex())?;
    let head = rpc.block_number().unwrap_or(0);
    let head_ts = rpc.block_timestamp().unwrap_or(now_secs());
    let now = now_secs();
    // An idle chain legitimately lags the wall clock (anvil, quiet networks), so
    // only reject a clock that is genuinely wrong. Epochs are wall-clock buckets,
    // so a badly skewed clock mines proofs that can never be submitted.
    let chain_behind = now as i64 - head_ts as i64;
    if chain_behind < -60 {
        return Err(format!(
            "the chain head is {}s ahead of this machine's clock; fix the clock first",
            -chain_behind
        ));
    }
    if chain_behind > 3600 {
        return Err(format!(
            "this machine's clock is {chain_behind}s ahead of the chain; fix the clock first"
        ));
    }

    println!();
    println!("contract   {contract_hex}  (chain {chain_id}, head {head})");
    println!("payout     {}", hex_addr(&payout));
    println!("gas wallet {}   balance {} wei", wallet.address_hex(), balance);
    println!("base       {base}   floor {floor}{}   threads {threads}", if measured.is_some() { " (auto)" } else { "" });
    println!("rpc        {}", cfg.rpc);
    if chain_behind > 120 {
        println!("note       head block is {chain_behind}s old; the next block follows the wall clock");
    }
    if balance == 0 {
        println!();
        println!("!! No ETH in the gas wallet. Mining still runs, but nothing can be");
        println!("!! submitted until you fund {} with a little ETH.", wallet.address_hex());
    }

    // --- run ---------------------------------------------------------------
    let (hit_tx, hit_rx) = mpsc::channel::<grind::Hit>();
    let submitter = Submitter {
        rpc: Rpc::new(&cfg.rpc),
        key: wallet.key,
        contract,
        contract_hex,
        from_hex: hex_addr(&wallet.address),
        chain_id,
        payout,
        floor,
        base,
        nonce: 0,
        ceiling_wei: (cfg.gas_ceiling_gwei.max(0.0) * GWEI as f64) as u128,
    };
    let _submit_handle = thread::spawn(move || submitter_loop(submitter, hit_rx));

    loop {
        let epoch = tx::epoch_at(now_secs());
        let chal = tx::challenge(&salt, epoch);
        let secs_left = 86_400 - (now_secs() % 86_400);
        println!();
        println!("epoch {epoch}   resets in {}   challenge 0x{}", hms(secs_left), hex::encode(chal));

        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let handles = grind::spawn_round(
            threads,
            tx::grind_prefix(&chal, &payout, floor),
            floor,
            epoch,
            run_start,
            Arc::clone(&stop),
            Arc::clone(&progress),
            hit_tx.clone(),
        );

        let mut last_count = 0u64;
        let mut last_tick = Instant::now();
        while tx::epoch_at(now_secs()) == epoch {
            thread::sleep(Duration::from_secs(2));
            let count = progress.load(Ordering::Relaxed);
            let dt = last_tick.elapsed().as_secs_f64();
            if dt >= 5.0 {
                let rate = (count - last_count) as f64 / dt / 1e6;
                println!("  {rate:.2} MH/s");
                last_count = count;
                last_tick = Instant::now();
            }
        }

        stop.store(true, Ordering::Relaxed);
        for h in handles {
            let _ = h.join();
        }
    }

    // Unreachable today; kept so the compiler is honest about the submitter.
    #[allow(unreachable_code)]
    {
        drop(hit_tx);
        let _ = _submit_handle.join();
        Ok(())
    }
}

// --- submission ------------------------------------------------------------

struct Submitter {
    rpc: Rpc,
    key: SigningKey,
    contract: [u8; 20],
    contract_hex: String,
    from_hex: String,
    chain_id: u64,
    payout: [u8; 20],
    floor: u8,
    base: u8,
    nonce: u64,
    ceiling_wei: u128,
}

fn submitter_loop(mut s: Submitter, hits: Receiver<grind::Hit>) {
    s.nonce = s.rpc.nonce(&s.from_hex).unwrap_or(0);

    while let Ok(hit) = hits.recv() {
        if hit.epoch != tx::epoch_at(now_secs()) {
            println!("  dropped a stale proof from epoch {}", hit.epoch);
            continue;
        }
        s.submit(hit);
    }
}

impl Submitter {
    fn submit(&mut self, hit: grind::Hit) {
        let reward: u128 = 1u128 << (hit.zeros - self.base as u32);
        let calldata = tx::mint_calldata(&[Submission {
            miner: self.payout,
            floor: self.floor,
            nonce: hit.nonce,
        }]);
        let data_hex = format!("0x{}", hex::encode(&calldata));

        let gas_price = self.rpc.gas_price().unwrap_or(20 * GWEI);
        let max_fee = (gas_price.saturating_mul(2)).min(self.ceiling_wei).max(1);
        if gas_price > self.ceiling_wei {
            println!("  gas price above ceiling, waiting one tick");
            thread::sleep(Duration::from_secs(12));
        }
        let priority = GWEI.min(max_fee);

        let gas_limit = self
            .rpc
            .estimate_gas(&self.from_hex, &self.contract_hex, &data_hex)
            .unwrap_or(FALLBACK_GAS);
        let gas_limit = gas_limit + gas_limit / 5; // 20% headroom

        let request = TxRequest {
            chain_id: self.chain_id,
            nonce: self.nonce,
            max_priority_fee: priority,
            max_fee,
            gas_limit,
            to: self.contract,
            data: calldata,
        };
        let (raw, expected_hash) = tx::sign_eip1559(&self.key, &request);
        let raw_hex = format!("0x{}", hex::encode(&raw));

        match self.rpc.send_raw_transaction(&raw_hex) {
            Ok(hash) => {
                self.nonce += 1;
                println!(
                    "  hit z={} -> {reward} SCRIP   tx {}",
                    hit.zeros,
                    if hash.is_empty() { format!("0x{}", hex::encode(expected_hash)) } else { hash }
                );
                if hit.twin {
                    println!("  dev twin: the house prints an equal note");
                }
            }
            Err(e) => {
                let lower = e.to_lowercase();
                if lower.contains("decode") {
                    // Keep the bytes: an unparseable transaction is a bug, not noise.
                    let _ = std::fs::write("failed-tx.hex", &raw_hex);
                    println!("  submit failed, raw tx saved to failed-tx.hex: {e}");
                } else if lower.contains("nonce") || lower.contains("replacement") {
                    if let Ok(n) = self.rpc.nonce(&self.from_hex) {
                        self.nonce = n;
                    }
                } else if lower.contains("funds") || lower.contains("balance") {
                    println!("  cannot submit: gas wallet is unfunded ({})", self.from_hex);
                } else {
                    println!("  submit failed: {e}");
                }
            }
        }
    }
}

// --- helpers ---------------------------------------------------------------

struct Args {
    config: Option<PathBuf>,
    contract: Option<String>,
    help: bool,
}

impl Args {
    fn parse(mut argv: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut args = Args { config: None, contract: None, help: false };
        while let Some(a) = argv.next() {
            match a.as_str() {
                "--config" => args.config = Some(PathBuf::from(argv.next().ok_or("--config needs a path")?)),
                "--contract" => args.contract = Some(argv.next().ok_or("--contract needs an address")?),
                "--help" | "-h" => args.help = true,
                other => return Err(format!("unknown argument {other} (try --help)")),
            }
        }
        Ok(args)
    }
}

fn print_help() {
    println!("scrip-miner {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("    scrip-miner [--config <path>] [--contract <address>]");
    println!();
    println!("On first run this creates config.toml and wallet.json in the working");
    println!("directory, then prints the address to fund with gas. Set `contract` in");
    println!("config.toml (or pass --contract) and start it again: it mines and submits");
    println!("on its own.");
}

fn selector(signature: &str) -> String {
    format!("0x{}", hex::encode(&tx::keccak256(signature.as_bytes())[..4]))
}

fn parse_address(s: &str) -> Result<[u8; 20], String> {
    let bytes = hex::decode(s.trim().trim_start_matches("0x"))
        .map_err(|e| format!("{s} is not a hex address: {e}"))?;
    if bytes.len() != 20 {
        return Err(format!("{s} is not a 20-byte address"));
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn resolve(dir: &Path, name: &str) -> PathBuf {
    let p = PathBuf::from(name);
    if p.is_absolute() {
        p
    } else {
        dir.join(p)
    }
}

fn hex_addr(a: &[u8; 20]) -> String {
    format!("0x{}", hex::encode(a))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hms(secs: u64) -> String {
    format!("{}h{:02}m{:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Measures this machine with the real grind loop, so the number reflects the
/// same code path (and core count) that will actually mine.
fn measure_hashrate(threads: usize, run_start: u64) -> f64 {
    const WARMUP_MS: u64 = 800;
    const SECS: u64 = 3;
    let stop = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(AtomicU64::new(0));
    let (tx, _rx) = mpsc::channel::<grind::Hit>();
    let handles = grind::spawn_round(
        threads,
        [0u8; 53],
        255, // unreachable rarity: count hashes, never produce a hit
        0,
        run_start,
        Arc::clone(&stop),
        Arc::clone(&progress),
        tx,
    );
    // Thread spawn and cold caches dominate the first few hundred ms; counting
    // them understates the rate and would pick a floor a bit too low.
    thread::sleep(Duration::from_millis(WARMUP_MS));
    progress.store(0, Ordering::Relaxed);
    thread::sleep(Duration::from_secs(SECS));

    let hashes = progress.load(Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = handle.join();
    }
    hashes as f64 / SECS as f64
}
/// Rarity that puts expected proofs per day as close to `target` as possible
/// without dropping below `base`. Rounds up, so it errs toward fewer, larger
/// notes rather than a stream of dust.
fn auto_floor(hashrate: f64, base: u8, target: f64) -> u8 {
    if !(hashrate > 0.0) || !(target > 0.0) {
        return base;
    }
    let hashes_per_proof = hashrate * 86_400.0 / target;
    let bits = hashes_per_proof.log2().ceil();
    if !bits.is_finite() {
        return base;
    }
    (bits as i64).clamp(base as i64, 128) as u8
}

#[cfg(test)]
mod tests {
    use super::auto_floor;

    fn rate(h: f64, floor: u8) -> f64 {
        h * 86_400.0 / 2f64.powi(floor as i32)
    }

    #[test]
    fn gpu_targeting_three_per_day_lands_at_45() {
        assert_eq!(auto_floor(1e9, 8, 3.0), 45);
    }

    #[test]
    fn reference_cpu_lands_in_the_thirties() {
        // 1.5 MH/s measured per core in this repo
        assert_eq!(auto_floor(1.5e6, 8, 3.0), 36);
    }

    #[test]
    fn never_goes_below_base_difficulty() {
        // A 1 GH/s machine wants 45; when the chain's base is already higher the
        // floor stays at the base.
        assert_eq!(auto_floor(1e9, 45, 3.0), 45);
        // 1 MH/s wants ~35, but base 50 forbids going that low.
        assert_eq!(auto_floor(1e6, 50, 3.0), 50);
    }

    #[test]
    fn degenerate_inputs_fall_back_to_base() {
        assert_eq!(auto_floor(0.0, 8, 3.0), 8);
        assert_eq!(auto_floor(1e9, 8, 0.0), 8);
        assert_eq!(auto_floor(f64::NAN, 8, 3.0), 8);
    }

    /// Rounding must never produce more than twice the requested rate, and
    /// never less than the target (it rounds up).
    #[test]
    fn chosen_floor_is_within_a_factor_of_two_of_target() {
        for h in [1e6, 1.5e6, 1e8, 1e9, 4e9] {
            for target in [0.5, 1.0, 3.0, 20.0] {
                let f = auto_floor(h, 1, target);
                let r = rate(h, f);
                assert!(r <= target, "h={h} t={target} f={f} r={r} overshoots");
                assert!(r > target / 2.0, "h={h} t={target} f={f} r={r} undershoots");
            }
        }
    }
}
