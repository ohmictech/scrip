//! Minimal JSON-RPC client. Blocking, one connection per call via a ureq agent.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub struct Rpc {
    url: String,
    id: AtomicU64,
    agent: ureq::Agent,
}

impl Rpc {
    pub fn new(url: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(25))
            .build();
        Self {
            url: url.to_string(),
            id: AtomicU64::new(1),
            agent,
        }
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();

        let response = self
            .agent
            .post(&self.url)
            .set("content-type", "application/json")
            .set("user-agent", "scrip-miner/0.1")
            .send_string(&body);

        let text = match response {
            Ok(r) => r.into_string().map_err(|e| e.to_string())?,
            Err(ureq::Error::Status(_, r)) => r.into_string().unwrap_or_default(),
            Err(e) => return Err(format!("transport: {e}")),
        };

        let value: Value =
            serde_json::from_str(&text).map_err(|e| format!("bad json ({e}): {text}"))?;
        if let Some(err) = value.get("error") {
            return Err(err.to_string());
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    pub fn chain_id(&self) -> Result<u64, String> {
        let v = self.call("eth_chainId", json!([]))?;
        parse_hex_u64(&v.as_str().ok_or("chainId: not a string")?).ok_or("chainId: bad hex".into())
    }

    pub fn block_number(&self) -> Result<u64, String> {
        let v = self.call("eth_blockNumber", json!([]))?;
        parse_hex_u64(&v.as_str().ok_or("blockNumber: not a string")?)
            .ok_or("blockNumber: bad hex".into())
    }

    /// Timestamp of the head block, used only for a clock sanity check.
    pub fn block_timestamp(&self) -> Result<u64, String> {
        let v = self.call("eth_getBlockByNumber", json!(["latest", false]))?;
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .ok_or("block: no timestamp")?;
        parse_hex(ts).map(|n| n as u64).ok_or_else(|| "block: bad timestamp".to_string())
    }

    pub fn balance(&self, address: &str) -> Result<u128, String> {
        let v = self.call("eth_getBalance", json!([address, "latest"]))?;
        Ok(parse_hex(&v.as_str().ok_or("balance: not a string")?).unwrap_or(0))
    }

    pub fn gas_price(&self) -> Result<u128, String> {
        let v = self.call("eth_gasPrice", json!([]))?;
        Ok(parse_hex(&v.as_str().ok_or("gasPrice: not a string")?).unwrap_or(0))
    }

    pub fn nonce(&self, address: &str) -> Result<u64, String> {
        let v = self.call("eth_getTransactionCount", json!([address, "pending"]))?;
        parse_hex_u64(&v.as_str().ok_or("nonce: not a string")?).ok_or("nonce: bad hex".into())
    }

    pub fn code(&self, address: &str) -> Result<String, String> {
        let v = self.call("eth_getCode", json!([address, "latest"]))?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    /// Read-only call returning the raw 32-byte result as hex.
    pub fn eth_call(&self, to: &str, data: &str) -> Result<String, String> {
        let v = self.call("eth_call", json!([{"to": to, "data": data}, "latest"]))?;
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "eth_call: no result".to_string())
    }

    pub fn estimate_gas(&self, from: &str, to: &str, data: &str) -> Result<u64, String> {
        let v = self.call(
            "eth_estimateGas",
            json!([{"from": from, "to": to, "data": data}]),
        )?;
        parse_hex_u64(&v.as_str().ok_or("estimateGas: not a string")?)
            .ok_or("estimateGas: bad hex".into())
    }

    pub fn send_raw_transaction(&self, raw_hex: &str) -> Result<String, String> {
        let v = self.call("eth_sendRawTransaction", json!([raw_hex]))?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }
}

/// Parse a 0x-prefixed hex quantity into u128. Tolerates empty / "0x".
pub fn parse_hex(s: &str) -> Option<u128> {
    let t = s.trim_start_matches("0x");
    if t.is_empty() {
        return Some(0);
    }
    u128::from_str_radix(t, 16).ok()
}

pub fn parse_hex_u64(s: &str) -> Option<u64> {
    parse_hex(s).map(|v| v as u64)
}

/// Decode a 32-byte hex word into a byte array.
pub fn word32(s: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(s.trim_start_matches("0x")).ok()?;
    if bytes.len() < 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[bytes.len() - 32..]);
    Some(out)
}
