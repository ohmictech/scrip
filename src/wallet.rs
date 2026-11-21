//! The funding wallet: a plain key file next to the config.
//!
//! Deliberately simple. This is a hot key that pays gas only; treat it that way.

use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct WalletFile {
    pub address: String,
    pub private_key: String,
    pub warning: String,
}

pub struct Wallet {
    pub key: SigningKey,
    pub address: [u8; 20],
}

impl Wallet {
    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.address))
    }
}

/// Load the key file, creating it with a fresh key if it does not exist.
pub fn load_or_create(path: &Path) -> Result<Wallet, String> {
    if path.exists() {
        let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let file: WalletFile =
            serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        let key = signing_key_from_hex(&file.private_key)?;
        let address = crate::tx::address_of(&key);
        return Ok(Wallet { key, address });
    }

    let key = fresh_key()?;
    let address = crate::tx::address_of(&key);
    let file = WalletFile {
        address: format!("0x{}", hex::encode(address)),
        private_key: format!("0x{}", hex::encode(key.to_bytes())),
        warning: "hot key for gas only; never the payout destination for large balances".into(),
    };
    let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(Wallet { key, address })
}

fn fresh_key() -> Result<SigningKey, String> {
    loop {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|e| format!("entropy: {e}"))?;
        if let Ok(key) = SigningKey::from_bytes(&bytes.into()) {
            return Ok(key);
        }
        // 2^-128 chance of an out-of-range scalar; just draw again.
    }
}

fn signing_key_from_hex(s: &str) -> Result<SigningKey, String> {
    let bytes = hex::decode(s.trim().trim_start_matches("0x"))
        .map_err(|e| format!("wallet key is not hex: {e}"))?;
    if bytes.len() != 32 {
        return Err("wallet key must be 32 bytes".into());
    }
    SigningKey::from_bytes(bytes.as_slice().into()).map_err(|_| "wallet key is out of range".into())
}
