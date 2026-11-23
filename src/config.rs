//! config.toml, created next to the binary on first run.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub rpc: String,
    /// 0 = one thread per available core.
    pub threads: usize,
    /// Address credited in proofs. Empty = the wallet address.
    pub payout: String,
    /// Wallet file used to pay gas, relative to the config file.
    pub wallet: String,
    /// Scrip contract address. Defaults to the canonical deployment.
    #[serde(default = "default_contract")]
    pub contract: String,
    /// 0 = auto-pick from measured hashrate; otherwise the minimum leading zeros.
    pub floor: u8,
    /// When `floor` is 0, aim for roughly this many proofs per day.
    #[serde(default = "default_target_proofs")]
    pub target_proofs_per_day: f64,
    /// auto = GPU when this build supports it and a device exists, else CPU.
    /// cpu / gpu force one engine. Defaults to cpu so upgrading the binary never
    /// silently changes how an existing install mines.
    #[serde(default = "default_device")]
    pub device: String,
    pub gas_ceiling_gwei: f64,
}

fn default_target_proofs() -> f64 {
    3.0
}

fn default_device() -> String {
    "auto".to_string()
}

/// Canonical Scrip deployment. Filled in automatically so a fresh install mines
/// without editing anything; on another chain, override `contract`.
pub const DEFAULT_CONTRACT: &str = "0x50F15a52d251Dfa33862eEe0b5BbB80f5283e9E9";

fn default_contract() -> String {
    DEFAULT_CONTRACT.to_string()
}

impl Config {
    pub fn template(wallet_address: &str) -> String {
        format!(
            r#"# Scrip miner configuration.
# Every value here can also be overridden with a flag: see `scrip-miner --help`.

# Ethereum JSON-RPC endpoint. The default is public and keyless; it is fine to
# start on, but use your own node for steady-state mining.
rpc = "https://ethereum-rpc.publicnode.com"

# Mining threads. 0 = every available core.
threads = 0

# Where mined tokens are credited. Proofs bind this address, so a relay cannot
# redirect them. Leave empty to use the wallet address below.
payout = "{wallet_address}"

# Hot key that pays gas. Created next to this file on first run. keep the balance low
wallet = "wallet.json"

# The Scrip contract address. Pre-filled with the canonical deployment.
contract = "{DEFAULT_CONTRACT}"

# Minimum rarity to submit. 0 = auto: measure this machine and pick a floor that
# yields about `target_proofs_per_day`. Set a number to force it.
floor = 0

# With floor = 0, aim for roughly this many proofs a day. Lower means bigger,
# rarer notes (less gas); higher means more, smaller ones.
target_proofs_per_day = 3.0

# Which engine grinds: auto (measure the CPU and the GPU, keep the faster one),
# cpu, gpu, or both (cpu and gpu together). GPU mining needs a binary built with
# the `gpu` feature; without one, auto simply uses the CPU.
device = "auto"

# Never pay more than this for gas, in gwei. Proofs are held, not dropped, while
# gas is above the ceiling; a proof still unsubmitted at the epoch boundary dies.
gas_ceiling_gwei = 5.0
"#
        )
    }
}

/// Returns the config and whether it had to be created.
pub fn load_or_create(path: &Path, wallet_address: &str) -> Result<(Config, bool), String> {
    if !path.exists() {
        let text = Config::template(wallet_address);
        fs::write(path, &text).map_err(|e| format!("write {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| format!("parse generated config: {e}"))?;
        return Ok((cfg, true));
    }

    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let cfg: Config = toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok((cfg, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_and_carries_the_canonical_contract() {
        let text = Config::template("0x0000000000000000000000000000000000000001");
        let cfg: Config = toml::from_str(&text).expect("generated template is valid toml");
        assert_eq!(cfg.contract, DEFAULT_CONTRACT);
        assert_eq!(cfg.floor, 0, "floor 0 means auto");
    }

    #[test]
    fn contract_falls_back_when_the_key_is_missing() {
        let text = r#"
rpc = "https://example.invalid"
threads = 2
payout = ""
wallet = "wallet.json"
floor = 0
target_proofs_per_day = 3.0
gas_ceiling_gwei = 5.0
"#;
        let cfg: Config = toml::from_str(text).expect("parses without a contract key");
        assert_eq!(cfg.contract, DEFAULT_CONTRACT);
    }

    #[test]
    fn an_explicit_contract_is_kept() {
        let text = r#"
rpc = "https://example.invalid"
threads = 2
payout = ""
wallet = "wallet.json"
contract = "0x0000000000000000000000000000000000000002"
floor = 0
gas_ceiling_gwei = 5.0
"#;
        let cfg: Config = toml::from_str(text).expect("parses");
        assert_eq!(cfg.contract, "0x0000000000000000000000000000000000000002");
    }
}
