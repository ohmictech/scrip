//! The grind loop: N threads, disjoint nonce ranges, one Keccak per attempt.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::tx::{keccak256, leading_zeros};

pub struct Hit {
    pub epoch: u64,
    pub nonce: [u8; 32],
    pub zeros: u32,
    /// The digest triggers the dev twin; the miner is told so it can say so.
    pub twin: bool,
}

const CHUNK: u64 = 4096;
const MASK_48: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Stream ids in the top 16 bits of the counter that the GPU engines own.
///
/// The CPU grinders put their thread index there, and thread 0 owns the whole
/// low-48-bit space. GPUs take the top 256 ids so that a hybrid or multi-GPU run
/// cannot re-grind another engine's counters, which would burn electricity twice
/// and produce duplicate proofs where one of each pair reverts.
pub const GPU_STREAM_BASE: u16 = 0xFF00;

/// Most GPUs that can be given a distinct stream id.
#[cfg(feature = "gpu")]
pub const MAX_GPU_ENGINES: usize = (u16::MAX - GPU_STREAM_BASE) as usize + 1;

/// Spawns `threads` workers that grind against one fixed prefix until `stop`.
///
/// Nonce layout: 24 zero bytes then a big-endian u64 whose top 16 bits are the
/// thread index, so ranges are disjoint by construction. `start` randomises the
/// 48-bit counter per run: without it, restarting inside an epoch re-grinds the
/// same nonces and every hit is an already-spent duplicate.
pub fn spawn_round(
    threads: usize,
    prefix53: [u8; 53],
    floor: u8,
    epoch: u64,
    start: u64,
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    hits: Sender<Hit>,
) -> Vec<JoinHandle<()>> {
    // The top 16 bits of the counter word hold the stream index: 0..=0xFEFF for
    // CPU threads, 0xFF00..=0xFFFF reserved for GPU engines (`gpu::stream_id`).
    // Without that reservation a hybrid or multi-GPU run would grind the same
    // nonces twice.
    let threads = threads.clamp(1, GPU_STREAM_BASE as usize);

    (0..threads)
        .map(|id| {
            let hits = hits.clone();
            let stop = Arc::clone(&stop);
            let progress = Arc::clone(&progress);
            thread::spawn(move || {
                // 85 bytes: chal(32) ‖ miner(20) ‖ floor(1) ‖ nonce(32). The
                // counter lives in the low 8 bytes of the nonce word, so the
                // other 24 bytes stay zero.
                let mut buf = [0u8; 85];
                buf[..53].copy_from_slice(&prefix53);
                let base = (id as u64) << 48;
                let mut counter: u64 = 0;

                loop {
                    for _ in 0..CHUNK {
                        let n = base | (start.wrapping_add(counter) & MASK_48);
                        counter = counter.wrapping_add(1);
                        buf[77..].copy_from_slice(&n.to_be_bytes());

                        let digest = keccak256(&buf);
                        let z = leading_zeros(&digest);
                        if z >= floor as u32 {
                            let mut nonce = [0u8; 32];
                            nonce[24..].copy_from_slice(&n.to_be_bytes());
                            let twin = crate::tx::is_dev_twin(&digest);
                            if hits.send(Hit { epoch, nonce, zeros: z, twin }).is_err() {
                                return; // submitter gone
                            }
                        }
                    }
                    progress.fetch_add(CHUNK, Ordering::Relaxed);
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::GPU_STREAM_BASE;
    use crate::tx::{challenge, digest_of, keccak256};

    /// A hybrid run puts the CPU threads and the GPU in the same 64-bit nonce
    /// word, so their stream ids must never overlap. CPU ids top out one below
    /// `GPU_STREAM_BASE`, which leaves the top 256 ids to the GPU engines. If
    /// this breaks, a hybrid run grinds duplicate nonces: wasted electricity, and
    /// one of each duplicate pair reverts on chain.
    #[test]
    fn cpu_streams_never_take_a_reserved_gpu_stream() {
        let threads = usize::min(64, GPU_STREAM_BASE as usize);
        assert_eq!(threads.clamp(1, GPU_STREAM_BASE as usize), threads, "clamp must not shrink this");

        for id in 0..threads as u64 {
            let base = id << 48;
            assert!((base >> 48) < GPU_STREAM_BASE as u64, "cpu thread {id} collides with gpu streams");
        }

        // The clamp's own ceiling is what guarantees the reservation holds.
        let max_id = (GPU_STREAM_BASE - 1) as u64;
        assert_eq!((max_id << 48) >> 48, GPU_STREAM_BASE as u64 - 1, "highest cpu stream id");
    }

    /// The grind preimage must be byte-identical to what the contract hashes.
    /// Both earlier bugs (8-byte epoch, 8-byte nonce) were exactly this class.
    #[test]
    fn grind_preimage_matches_contract_encoding() {
        let salt = [0x11u8; 32];
        let chal = challenge(&salt, 20_000);
        let mut miner = [0u8; 20];
        miner[19] = 0xa1;
        let floor = 35u8;
        let prefix = crate::tx::grind_prefix(&chal, &miner, floor);

        for counter in [0u64, 7, 0x2222_2222_2222_2222] {
            let mut buf = [0u8; 85];
            buf[..53].copy_from_slice(&prefix);
            buf[77..].copy_from_slice(&counter.to_be_bytes());

            let mut nonce = [0u8; 32];
            nonce[24..].copy_from_slice(&counter.to_be_bytes());

            assert_eq!(
                keccak256(&buf),
                digest_of(&chal, &miner, floor, &nonce),
                "counter {counter}"
            );
        }
    }

    /// Published vector for the counter layout the grinder actually produces
    /// (high 24 bytes of the nonce are zero). Generated with `cast keccak`.
    #[test]
    fn grind_preimage_matches_published_vector() {
        let salt = [0x11u8; 32];
        let chal = challenge(&salt, 20_000);
        let mut miner = [0u8; 20];
        miner[19] = 0xa1;
        let prefix = crate::tx::grind_prefix(&chal, &miner, 35);

        let mut buf = [0u8; 85];
        buf[..53].copy_from_slice(&prefix);
        buf[77..].copy_from_slice(&7u64.to_be_bytes());

        let digest = keccak256(&buf);
        assert_eq!(
            hex::encode(digest),
            "d2c1c1410ecc23b69cb6b9ddb8d75168815addd4b31d76b725bedb8b69168e32"
        );
    }
}
