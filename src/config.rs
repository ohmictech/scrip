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
    /// Scrip contract address. Required.
    pub contract: String,
    /// 0 = auto-pick from measured hashrate; otherwise the minimum leading zeros.
    pub floor: u8,
    /// When `floor` is 0, aim for roughly this many proofs per day.
    #[serde(default = "default_target_proofs")]
    pub target_proofs_per_day: f64,
    pub gas_ceiling_gwei: f64,
}

fn default_target_proofs() -> f64 {
    3.0
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

# The Scrip contract address. REQUIRED - mining will not start without it.
contract = ""

# Minimum rarity to submit. 0 = auto: measure this machine and pick a floor that
# yields about `target_proofs_per_day`. Set a number to force it.
floor = 0

# With floor = 0, aim for roughly this many proofs a day. Lower means bigger,
# rarer notes (less gas); higher means more, smaller ones.
target_proofs_per_day = 3.0

# Never pay more than this for gas, in gwei.
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
