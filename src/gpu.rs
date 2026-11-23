//! Optional OpenCL backend.
//!
//! Compiled only with `--features gpu`, so the default build needs no GPU
//! runtime and the release archives stay dependency-free.
//!
//! The kernel is a single-block Keccak-256 (see `keccak_grind.cl`). The host owns
//! everything that touches keys or the network: the GPU only reports nonces and
//! their claimed rarity, exactly like the CPU grinders, and every hit is
//! re-verified on the CPU before submission. A buggy or hostile device can waste
//! work but cannot make the miner submit an invalid proof.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;

use opencl3::command_queue::CommandQueue;
use opencl3::context::Context;
use opencl3::device::{get_all_devices, Device, CL_DEVICE_TYPE_GPU};
use opencl3::kernel::{ExecuteKernel, Kernel};
use opencl3::memory::{Buffer, CL_MEM_READ_ONLY, CL_MEM_READ_WRITE, CL_MEM_WRITE_ONLY};
use opencl3::program::Program;
use opencl3::types::{cl_device_id, CL_BLOCKING};

use crate::grind::Hit;
use crate::tx;

const KERNEL_SRC: &str = include_str!("keccak_grind.cl");

/// Slots for hits reported by one launch. Hits are rare by construction, so this
/// is far above any plausible batch size.
const MAX_HITS: usize = 64;

/// Attempts per work-item per launch. Tuned so one launch is a few tens of
/// milliseconds, which keeps the epoch-boundary stop responsive.
const ITERS: u32 = 2_048;

/// The nonce stream word for GPU engine `index`.
///
/// Every engine needs a distinct value here, otherwise two GPUs (or a GPU and a
/// CPU thread) sweep the same low-48-bit counters: the work is done twice, and of
/// each duplicate pair one submission reverts and still costs gas.
pub fn stream_id(index: usize) -> u64 {
    ((crate::grind::GPU_STREAM_BASE as u64) + index as u64) << 48
}

struct Gpu {
    context: Context,
    queue: CommandQueue,
    kernel: Kernel,
    prefix: Buffer<u8>,
    nonces: Buffer<u64>,
    zeros: Buffer<u32>,
    count: Buffer<u32>,
    work_items: usize,
    stream: u64,
    name: String,
}

impl Gpu {
    /// Opens one specific device. Each device gets its own context and compiled
    /// program, since an OpenCL program is bound to the context that built it.
    fn open(index: usize, id: cl_device_id) -> Result<Gpu, String> {
        let device = Device::new(id);

        let name = device.name().unwrap_or_else(|_| "unknown".into());
        let context = Context::from_device(&device).map_err(|e| format!("create context for {name}: {e}"))?;
        let queue = CommandQueue::create_default(&context, 0).map_err(|e| format!("create queue for {name}: {e}"))?;

        // opencl3 appends the kernel build log to this error, which is the only
        // useful diagnostic when the OpenCL C fails to compile.
        let program = Program::create_and_build_from_source(&context, KERNEL_SRC, "")
            .map_err(|e| format!("build kernel for {name}: {e}"))?;
        let kernel = Kernel::create(&program, "grind").map_err(|e| format!("create kernel for {name}: {e}"))?;

        let prefix = unsafe { Buffer::<u8>::create(&context, CL_MEM_READ_ONLY, 53, ptr::null_mut()) }
            .map_err(|e| format!("alloc prefix: {e}"))?;
        let nonces = unsafe { Buffer::<u64>::create(&context, CL_MEM_WRITE_ONLY, MAX_HITS, ptr::null_mut()) }
            .map_err(|e| format!("alloc nonces: {e}"))?;
        let zeros = unsafe { Buffer::<u32>::create(&context, CL_MEM_WRITE_ONLY, MAX_HITS, ptr::null_mut()) }
            .map_err(|e| format!("alloc zeros: {e}"))?;
        let count = unsafe { Buffer::<u32>::create(&context, CL_MEM_READ_WRITE, 1, ptr::null_mut()) }
            .map_err(|e| format!("alloc count: {e}"))?;

        let work_items = work_items_for(&device);
        let stream = stream_id(index);
        Ok(Gpu { context, queue, kernel, prefix, nonces, zeros, count, work_items, stream, name })
    }

    fn write_prefix(&mut self, prefix53: &[u8; 53]) -> Result<(), String> {
        unsafe { self.queue.enqueue_write_buffer(&mut self.prefix, CL_BLOCKING, 0, prefix53, &[]) }
            .map(|_| ())
            .map_err(|e| format!("upload prefix: {e}"))
    }

    fn launch(&mut self, start: u64, floor: u8) -> Result<Vec<(u64, u32)>, String> {
        unsafe { self.queue.enqueue_write_buffer(&mut self.count, CL_BLOCKING, 0, &[0u32], &[]) }
            .map_err(|e| format!("reset hit counter: {e}"))?;

        let stride = self.work_items as u64;
        let floor_bits = floor as u32;
        let max_hits = MAX_HITS as u32;
        unsafe {
            let mut exec = ExecuteKernel::new(&self.kernel);
            exec.set_arg(&self.prefix)
                .set_arg(&self.stream)
                .set_arg(&start)
                .set_arg(&stride)
                .set_arg(&ITERS)
                .set_arg(&floor_bits)
                .set_arg(&self.nonces)
                .set_arg(&self.zeros)
                .set_arg(&self.count)
                .set_arg(&max_hits);
            exec.set_global_work_size(self.work_items);
            exec.enqueue_nd_range(&self.queue).map_err(|e| format!("enqueue: {e}"))?;
        }
        self.queue.finish().map_err(|e| format!("finish: {e}"))?;

        let mut n_hits = [0u32; 1];
        unsafe { self.queue.enqueue_read_buffer(&self.count, CL_BLOCKING, 0, &mut n_hits, &[]) }
            .map_err(|e| format!("read hit count: {e}"))?;
        let n = (n_hits[0] as usize).min(MAX_HITS);
        if n == 0 {
            return Ok(Vec::new());
        }

        let mut ns = vec![0u64; n];
        let mut zs = vec![0u32; n];
        unsafe {
            self.queue
                .enqueue_read_buffer(&self.nonces, CL_BLOCKING, 0, &mut ns[..n], &[])
                .map_err(|e| format!("read nonces: {e}"))?;
            self.queue
                .enqueue_read_buffer(&self.zeros, CL_BLOCKING, 0, &mut zs[..n], &[])
                .map_err(|e| format!("read zeros: {e}"))?;
        }
        Ok(ns.into_iter().zip(zs).collect())
    }
}

/// The grid size for one device. A GPU has no notion of the `threads` setting,
/// which counts CPU cores, so the work is sized from the hardware instead.
fn work_items_for(device: &Device) -> usize {
    let units = device.max_compute_units().unwrap_or(8) as usize;
    (units * 256).clamp(1_024, 262_144)
}

/// Every GPU this machine exposes, across all OpenCL platforms, capped at the
/// number that can be given a distinct nonce stream.
fn gpu_ids() -> Result<Vec<cl_device_id>, String> {
    let mut ids = get_all_devices(CL_DEVICE_TYPE_GPU).map_err(|e| format!("enumerate devices: {e}"))?;
    if ids.is_empty() {
        return Err("no OpenCL GPU device on this machine".into());
    }
    let cap = crate::grind::MAX_GPU_ENGINES;
    if ids.len() > cap {
        eprintln!("  gpu: found {} devices, using the first {cap} (nonce stream limit)", ids.len());
        ids.truncate(cap);
    }
    Ok(ids)
}

/// Human-readable summary of the devices that would be used.
pub fn info() -> Result<String, String> {
    let ids = gpu_ids()?;
    let mut parts = Vec::new();
    let mut total = 0usize;
    for id in &ids {
        let d = Device::new(*id);
        let name = d.name().unwrap_or_else(|_| "unknown".into());
        let w = work_items_for(&d);
        total += w;
        parts.push(format!("{name} ({w} work-items)"));
    }
    Ok(if parts.len() == 1 {
        parts.remove(0)
    } else {
        format!("{} gpus: {}, {total} work-items total", parts.len(), parts.join(" + "))
    })
}

/// Lists every OpenCL platform and device this machine exposes.
///
/// Vendors install their own ICD, so a missing runtime shows up here as "no
/// platforms" rather than as a confusing failure to mine. OpenCL CPU devices are
/// listed too, but the backend ignores them: the CPU grinders already cover that
/// ground more efficiently than an OpenCL CPU implementation would.
pub fn describe_devices() -> String {
    let platforms = match opencl3::platform::get_platforms() {
        Ok(p) => p,
        Err(e) => return format!("no OpenCL platforms ({e}). Install or reinstall the GPU driver."),
    };
    if platforms.is_empty() {
        return "no OpenCL platforms. Install or reinstall the GPU driver.".into();
    }

    let mut out = String::new();
    for (i, p) in platforms.iter().enumerate() {
        let name = p.name().unwrap_or_else(|_| "unknown".into());
        let version = p.version().unwrap_or_else(|_| "?".into());
        out.push_str(&format!("platform {i}  {name}  ({version})\n"));

        match p.get_devices(opencl3::device::CL_DEVICE_TYPE_ALL) {
            Ok(ids) if !ids.is_empty() => {
                for id in ids {
                    let d = Device::new(id);
                    let dname = d.name().unwrap_or_else(|_| "unknown".into());
                    let kind = match d.dev_type() {
                        Ok(t) if t & CL_DEVICE_TYPE_GPU != 0 => "gpu",
                        Ok(t) if t & opencl3::device::CL_DEVICE_TYPE_CPU != 0 => "cpu",
                        _ => "other",
                    };
                    let units = d.max_compute_units().unwrap_or(0);
                    out.push_str(&format!("    {kind:<5} {dname}  ({units} compute units)\n"));
                }
            }
            Ok(_) => out.push_str("    no devices\n"),
            Err(e) => out.push_str(&format!("    device query failed: {e}\n")),
        }
    }
    out.trim_end().to_string()
}

/// Grinds on the GPU in a worker thread, reporting hits on the same channel the
/// CPU grinders use so the submitter needs no special case.
pub fn spawn_round(
    prefix53: [u8; 53],
    floor: u8,
    epoch: u64,
    start: u64,
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    hits: Sender<Hit>,
) -> Vec<JoinHandle<()>> {
    let ids = match gpu_ids() {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("  gpu: {e}");
            return Vec::new();
        }
    };

    // One thread per device. They share the stop flag, the progress counter and
    // the hit channel, so a single submitter still serves every engine, and one
    // device failing does not stop the others.
    ids.into_iter()
        .enumerate()
        .map(|(index, id)| {
            let stop = Arc::clone(&stop);
            let progress = Arc::clone(&progress);
            let hits = hits.clone();
            // `cl_device_id` is a raw pointer, which is not `Send`, so it crosses
            // the thread boundary as a plain integer and is cast back inside.
            let raw = id as usize;
            std::thread::spawn(move || {
                let id = raw as cl_device_id;
                if let Err(e) = run(prefix53, floor, epoch, start, stop, progress, hits, index, id) {
                    eprintln!("  gpu[{index}]: {e}");
                    eprintln!("  gpu[{index}]: stopped; other engines keep running");
                }
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn run(
    prefix53: [u8; 53],
    floor: u8,
    epoch: u64,
    start: u64,
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    hits: Sender<Hit>,
    index: usize,
    id: cl_device_id,
) -> Result<(), String> {
    let mut g = Gpu::open(index, id)?;
    g.write_prefix(&prefix53)?;

    // Rebuilt from the prefix so every hit is verified rather than trusted: the
    // prefix is chal(32) || miner(20) || floor(1).
    let mut chal = [0u8; 32];
    chal.copy_from_slice(&prefix53[..32]);
    let mut miner = [0u8; 20];
    miner.copy_from_slice(&prefix53[32..52]);

    let per_launch = (g.work_items as u64) * (ITERS as u64);
    let mut offset = start;
    while !stop.load(Ordering::Relaxed) {
        let found = g.launch(offset, floor)?;
        offset = offset.wrapping_add(per_launch);
        progress.fetch_add(per_launch, Ordering::Relaxed);

        for (n, z) in found {
            let mut nonce = [0u8; 32];
            nonce[24..].copy_from_slice(&n.to_be_bytes());

            // Never trust the device: recompute the digest and re-check rarity.
            let digest = tx::digest_of(&chal, &miner, floor, &nonce);
            let real = tx::leading_zeros(&digest);
            if real < floor as u32 {
                eprintln!("  gpu: discarded a bogus hit (claimed z={z}, real z={real})");
                continue;
            }

            let hit = Hit { epoch, nonce, zeros: real, twin: tx::is_dev_twin(&digest) };
            if hits.send(hit).is_err() {
                return Ok(()); // submitter gone
            }
        }
    }
    Ok(())
}

/// Compares OpenCL digests against the CPU implementation for a spread of
/// counters, so a broken kernel is caught before it wastes an epoch. Runs on
/// every GPU present.
pub fn selftest() -> Result<String, String> {
    let ids = gpu_ids()?;
    let mut reports = Vec::new();
    for (index, id) in ids.into_iter().enumerate() {
        reports.push(selftest_one(index, id)?);
    }
    Ok(reports.join("; "))
}

fn selftest_one(index: usize, id: cl_device_id) -> Result<String, String> {
    let g = Gpu::open(index, id)?;
    let program = Program::create_and_build_from_source(&g.context, KERNEL_SRC, "")
        .map_err(|e| format!("build for selftest: {e}"))?;
    let k = Kernel::create(&program, "digest_of").map_err(|e| format!("kernel digest_of: {e}"))?;

    let salt = [0x11u8; 32];
    let mut miner = [0u8; 20];
    miner[19] = 0xa1;
    let floor = 30u8;
    let chal = tx::challenge(&salt, 20_000);
    let prefix = tx::grind_prefix(&chal, &miner, floor);

    let n = 512usize;
    let counters: Vec<u64> = (0..n as u64).map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).collect();

    let mut pbuf = unsafe { Buffer::<u8>::create(&g.context, CL_MEM_READ_ONLY, 53, ptr::null_mut()) }
        .map_err(|e| format!("alloc prefix: {e}"))?;
    let mut cbuf = unsafe { Buffer::<u64>::create(&g.context, CL_MEM_READ_ONLY, n, ptr::null_mut()) }
        .map_err(|e| format!("alloc counters: {e}"))?;
    let obuf = unsafe { Buffer::<u8>::create(&g.context, CL_MEM_WRITE_ONLY, n * 32, ptr::null_mut()) }
        .map_err(|e| format!("alloc out: {e}"))?;

    unsafe {
        g.queue
            .enqueue_write_buffer(&mut pbuf, CL_BLOCKING, 0, &prefix, &[])
            .map_err(|e| format!("upload prefix: {e}"))?;
        g.queue
            .enqueue_write_buffer(&mut cbuf, CL_BLOCKING, 0, &counters, &[])
            .map_err(|e| format!("upload counters: {e}"))?;
        let mut exec = ExecuteKernel::new(&k);
        exec.set_arg(&pbuf).set_arg(&cbuf).set_arg(&obuf);
        exec.set_global_work_size(n);
        exec.enqueue_nd_range(&g.queue).map_err(|e| format!("enqueue: {e}"))?;
    }
    g.queue.finish().map_err(|e| format!("finish: {e}"))?;

    let mut out = vec![0u8; n * 32];
    unsafe { g.queue.enqueue_read_buffer(&obuf, CL_BLOCKING, 0, &mut out, &[]) }
        .map_err(|e| format!("read digests: {e}"))?;

    for (i, c) in counters.iter().enumerate() {
        let mut nonce = [0u8; 32];
        nonce[24..].copy_from_slice(&c.to_be_bytes());
        let expect = tx::digest_of(&chal, &miner, floor, &nonce);
        let got = &out[i * 32..i * 32 + 32];
        if got != expect.as_slice() {
            return Err(format!(
                "digest mismatch at counter {i}: gpu {} cpu {}",
                hex::encode(got),
                hex::encode(expect)
            ));
        }
    }

    Ok(format!("{n} digests match the CPU implementation on {}", g.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives one real launch at a low floor so hits are plentiful, then checks
    /// every reported nonce against the CPU implementation. This is the path the
    /// mining loop depends on, and at the mainnet floor it would take hours to
    /// exercise by waiting for a real hit.
    #[test]
    fn reported_hits_are_valid() {
        let (index, id) = match gpu_ids().map(|mut v| (0usize, v.remove(0))) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };
        let mut g = match Gpu::open(index, id) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipping: {e}");
                return;
            }
        };

        let salt = [0x22u8; 32];
        let mut miner = [0u8; 20];
        miner[0..4].copy_from_slice(&[1, 2, 3, 4]);
        let floor = 8u8;
        let chal = tx::challenge(&salt, 12_345);
        let prefix = tx::grind_prefix(&chal, &miner, floor);
        g.write_prefix(&prefix).expect("upload prefix");

        let start = 0x0000_1234_5678_9abc_u64;
        let found = g.launch(start, floor).expect("launch");
        assert!(!found.is_empty(), "floor 8 over {} attempts should find hits", g.work_items * ITERS as usize);

        let span = (g.work_items as u64) * (ITERS as u64);
        const LOW48: u64 = 0x0000_FFFF_FFFF_FFFF;
        let expected_hi = g.stream >> 48;
        for (n, claimed) in found {
            let mut nonce = [0u8; 32];
            nonce[24..].copy_from_slice(&n.to_be_bytes());
            let digest = tx::digest_of(&chal, &miner, floor, &nonce);
            let real = tx::leading_zeros(&digest);

            assert!(real >= floor as u32, "hit below floor: z={real}");
            assert_eq!(real, claimed, "kernel and CPU disagree on rarity");
            assert_eq!(n >> 48, expected_hi, "kernel ignored the engine's reserved stream");

            // The counter must sit inside the swept window, once the stream bits
            // are masked off; the difference is taken modulo 2^48 so the
            // wrapping window is handled the same way the kernel handles it.
            let low = n & LOW48;
            let delta = low.wrapping_sub(start) & LOW48;
            assert!(delta < span, "nonce outside the swept range");
        }
    }

    /// Every GPU must get a distinct nonce stream, and none may overlap the CPU
    /// threads. Two engines sharing a stream grind the same counters: double the
    /// electricity, and one of each duplicate pair reverts on chain.
    #[test]
    fn each_gpu_gets_a_distinct_stream() {
        let mut seen = std::collections::HashSet::new();
        for index in 0..crate::grind::MAX_GPU_ENGINES {
            let hi = stream_id(index) >> 48;
            assert!(
                hi >= crate::grind::GPU_STREAM_BASE as u64,
                "gpu {index} intrudes on cpu stream space"
            );
            assert!(seen.insert(hi), "gpu {index} reuses stream {hi}");
        }

        // The CPU side is clamped to GPU_STREAM_BASE, so its highest id is one
        // below the first GPU stream.
        let highest_cpu = (crate::grind::GPU_STREAM_BASE - 1) as u64;
        assert!(!seen.contains(&highest_cpu), "cpu streams overlap the first gpu stream");
    }

    /// Every nonce the kernel sweeps must be distinct, otherwise the miner burns
    /// work re-grinding counters it already tried.
    #[test]
    fn launch_windows_advance() {
        let work_items = 1024u64;
        let per_launch = work_items * ITERS as u64;
        let start = 0x0000_0000_0000_0001u64;

        let mut seen = std::collections::HashSet::new();
        let mut offset = start;
        for _ in 0..3 {
            for i in 0..work_items {
                let n = (offset + i) & 0x0000_FFFF_FFFF_FFFF;
                assert!(seen.insert(n), "counter {n} reused");
            }
            offset = offset.wrapping_add(per_launch);
        }
    }
}
