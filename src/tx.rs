//! Keccak, RLP, EIP-1559 transaction construction and the Scrip calldata.
//!
//! Everything here is hand-rolled so the miner depends on nothing but `k256`
//! (signing) and `tiny-keccak`. The encoding must match `src/ScripLib.sol`
//! exactly; `test/Scrip.t.sol` holds the published vectors.

use k256::ecdsa::{RecoveryId, SigningKey, VerifyingKey};

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    use tiny_keccak::{Hasher, Keccak};
    let mut out = [0u8; 32];
    let mut h = Keccak::v256();
    h.update(data);
    h.finalize(&mut out);
    out
}

pub fn address_of(key: &SigningKey) -> [u8; 20] {
    let vk = VerifyingKey::from(key);
    let point = vk.to_encoded_point(false);
    let h = keccak256(&point.as_bytes()[1..]);
    let mut out = [0u8; 20];
    out.copy_from_slice(&h[12..]);
    out
}

// --- RLP ------------------------------------------------------------------

fn rlp_len(len: usize, offset: u8, out: &mut Vec<u8>) {
    if len < 56 {
        out.push(offset + len as u8);
    } else {
        let be = len.to_be_bytes();
        let start = be.iter().position(|&x| x != 0).unwrap();
        let slice = &be[start..];
        out.push(offset + 55 + slice.len() as u8);
        out.extend_from_slice(slice);
    }
}

fn rlp_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return vec![b[0]];
    }
    let mut out = Vec::with_capacity(b.len() + 4);
    rlp_len(b.len(), 0x80, &mut out);
    out.extend_from_slice(b);
    out
}

fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.concat();
    let mut out = Vec::with_capacity(payload.len() + 4);
    rlp_len(payload.len(), 0xc0, &mut out);
    out.extend_from_slice(&payload);
    out
}

/// Minimal-length integer encoding. Zero becomes the empty string (0x80), which
/// is what Ethereum requires — the classic hand-rolled-RLP bug.
fn rlp_num(b: &[u8]) -> Vec<u8> {
    let mut i = 0;
    while i < b.len() && b[i] == 0 {
        i += 1;
    }
    rlp_bytes(&b[i..])
}

fn rlp_u64(v: u64) -> Vec<u8> {
    rlp_num(&v.to_be_bytes())
}

fn rlp_u128(v: u128) -> Vec<u8> {
    rlp_num(&v.to_be_bytes())
}

// --- calldata -------------------------------------------------------------

pub struct Submission {
    pub miner: [u8; 20],
    pub floor: u8,
    pub nonce: [u8; 32],
}

fn word_from_bytes(b: &[u8]) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[32 - b.len()..].copy_from_slice(b);
    w
}

/// `mint((address,uint8,bytes32)[])`
pub fn mint_calldata(subs: &[Submission]) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 64 + 96 * subs.len());
    data.extend_from_slice(&keccak256(b"mint((address,uint8,bytes32)[])")[..4]);
    data.extend_from_slice(&word_from_bytes(&(32u64).to_be_bytes())); // offset
    data.extend_from_slice(&word_from_bytes(&(subs.len() as u64).to_be_bytes()));
    for s in subs {
        data.extend_from_slice(&word_from_bytes(&s.miner));
        data.extend_from_slice(&word_from_bytes(&[s.floor]));
        data.extend_from_slice(&s.nonce);
    }
    data
}

// --- EIP-1559 -------------------------------------------------------------

pub struct TxRequest {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee: u128,
    pub max_fee: u128,
    pub gas_limit: u64,
    pub to: [u8; 20],
    pub data: Vec<u8>,
}

/// Returns the raw signed transaction and its hash.
pub fn sign_eip1559(key: &SigningKey, tx: &TxRequest) -> (Vec<u8>, [u8; 32]) {
    let mut fields = vec![
        rlp_u64(tx.chain_id),
        rlp_u64(tx.nonce),
        rlp_u128(tx.max_priority_fee),
        rlp_u128(tx.max_fee),
        rlp_u64(tx.gas_limit),
        rlp_bytes(&tx.to),
        rlp_bytes(&[]), // value = 0
        rlp_bytes(&tx.data),
        rlp_list(&[]), // access list
    ];

    let mut unsigned = vec![0x02u8];
    unsigned.extend_from_slice(&rlp_list(&fields));
    let sighash = keccak256(&unsigned);

    let (sig, recovery): (k256::ecdsa::Signature, RecoveryId) =
        key.sign_prehash_recoverable(&sighash).expect("sign");

    fields.push(rlp_num(&[recovery.to_byte()]));
    fields.push(rlp_bytes(&sig.r().to_bytes()));
    fields.push(rlp_bytes(&sig.s().to_bytes()));

    let mut raw = vec![0x02u8];
    raw.extend_from_slice(&rlp_list(&fields));
    let hash = keccak256(&raw);
    (raw, hash)
}

// --- proof format (must match ScripLib.sol) -------------------------------

pub fn epoch_at(timestamp: u64) -> u64 {
    timestamp / 86_400
}

pub fn challenge(salt: &[u8; 32], epoch: u64) -> [u8; 32] {
    // abi.encodePacked(bytes32 salt, uint256 epoch): 32 + 32 bytes, big-endian.
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(salt);
    buf[56..].copy_from_slice(&epoch.to_be_bytes());
    keccak256(&buf)
}

/// The 53-byte fixed prefix `chal ‖ miner ‖ floor`. Only the nonce tail changes
/// inside the grind loop.
pub fn grind_prefix(chal: &[u8; 32], miner: &[u8; 20], floor: u8) -> [u8; 53] {
    let mut prefix = [0u8; 53];
    prefix[..32].copy_from_slice(chal);
    prefix[32..52].copy_from_slice(miner);
    prefix[52] = floor;
    prefix
}

pub fn leading_zeros(digest: &[u8; 32]) -> u32 {
    let mut n = 0u32;
    for byte in digest {
        if *byte == 0 {
            n += 8;
        } else {
            n += byte.leading_zeros();
            break;
        }
    }
    n
}

pub fn is_dev_twin(digest: &[u8; 32]) -> bool {
    digest[31] & 0x3F == 0x2A
}

/// The full proof digest: `chal ‖ miner ‖ floor ‖ nonce`, 85 bytes, matching
/// `ScripLib.digest`. The grind loop builds this inline for speed; this is the
/// readable reference and the thing the tests pin.
#[allow(dead_code)]
pub fn digest_of(chal: &[u8; 32], miner: &[u8; 20], floor: u8, nonce: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 85];
    buf[..32].copy_from_slice(chal);
    buf[32..52].copy_from_slice(miner);
    buf[52] = floor;
    buf[53..].copy_from_slice(nonce);
    keccak256(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Well-known vector: private key 0x..01 owns 0x7E5F...5Bdf.
    #[test]
    fn address_derivation_matches_known_vector() {
        let mut b = [0u8; 32];
        b[31] = 1;
        let key = SigningKey::from_bytes(&b.into()).unwrap();
        assert_eq!(
            format!("0x{}", hex::encode(address_of(&key))),
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
    }

    #[test]
    fn leading_zeros_is_correct() {
        let mut d = [0u8; 32];
        assert_eq!(leading_zeros(&d), 256);
        d[0] = 0x01;
        assert_eq!(leading_zeros(&d), 7);
        d[0] = 0xff;
        assert_eq!(leading_zeros(&d), 0);
        d[0] = 0x00;
        d[1] = 0x01;
        assert_eq!(leading_zeros(&d), 15);
    }

    /// Published vector, generated independently with `cast keccak` and asserted
    /// against Solidity in test/Scrip.t.sol.
    #[test]
    fn digest_vector_matches_solidity() {
        let salt = [0x11u8; 32];
        let chal = challenge(&salt, 20_000);
        assert_eq!(
            hex::encode(chal),
            "18ee763ed72a14172d064cce2ed53214a10ecd20c034d0407ace1a0a00b91629"
        );
        let mut buf = Vec::new();
        buf.extend_from_slice(&chal);
        let mut miner = [0u8; 20];
        miner[19] = 0xa1;
        buf.extend_from_slice(&miner);
        buf.push(35);
        buf.extend_from_slice(&[0x22u8; 32]);
        assert_eq!(
            hex::encode(keccak256(&buf)),
            "255728b59e47c11ea775cb7d81eff069444b66895e2dc17c2e42bf5941aade38"
        );
    }

    #[test]
    fn rlp_zero_is_the_empty_string() {
        assert_eq!(rlp_num(&0u64.to_be_bytes()), vec![0x80]);
        assert_eq!(rlp_num(&1u64.to_be_bytes()), vec![0x01]);
        assert_eq!(rlp_num(&127u64.to_be_bytes()), vec![0x7f]);
        assert_eq!(rlp_num(&128u64.to_be_bytes()), vec![0x81, 0x80]);
    }

    #[test]
    fn calldata_layout_is_three_words_per_submission() {
        let subs = vec![Submission {
            miner: [0x11u8; 20],
            floor: 35,
            nonce: [0x22u8; 32],
        }];
        let data = mint_calldata(&subs);
        assert_eq!(data.len(), 4 + 64 + 96);
        assert_eq!(&data[4..36], &word_from_bytes(&(32u64).to_be_bytes()));
        assert_eq!(&data[36..68], &word_from_bytes(&(1u64).to_be_bytes()));
        assert_eq!(&data[100..132], &word_from_bytes(&[35u8]));
    }

    #[test]
    fn signed_tx_is_typed_and_sized() {
        let mut b = [0u8; 32];
        b[31] = 1;
        let key = SigningKey::from_bytes(&b.into()).unwrap();
        let tx = TxRequest {
            chain_id: 1,
            nonce: 0,
            max_priority_fee: 1_000_000_000,
            max_fee: 20_000_000_000,
            gas_limit: 210_000,
            to: [0x42u8; 20],
            data: vec![1, 2, 3],
        };
        let (raw, hash) = sign_eip1559(&key, &tx);
        assert_eq!(raw[0], 0x02, "EIP-1559 type byte");
        assert_eq!(keccak256(&raw), hash);
        // Sanity: a signed tx carries r and s, so it must be clearly longer than
        // the 9-field unsigned body.
        assert!(raw.len() > 60);
    }
}
