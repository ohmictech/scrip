// OpenCL Keccak-256 grind for Scrip proofs.
//
// The message is always 85 bytes: chal(32) || miner(20) || floor(1) || nonce(32),
// where the nonce is 24 zero bytes followed by a big-endian u64 counter. That
// fits inside Keccak-256's 136-byte rate, so every attempt is exactly one
// permutation. Only the 8 counter bytes and the two padding bytes differ between
// attempts, so the block is built once per work-item and patched in the loop.

#define ROTL64(x, n) (((x) << (n)) | ((x) >> (64 - (n))))

#define MASK48 0x0000FFFFFFFFFFFFUL

__constant ulong RC[24] = {
    0x0000000000000001UL, 0x0000000000008082UL, 0x800000000000808AUL, 0x8000000080008000UL,
    0x000000000000808BUL, 0x0000000080000001UL, 0x8000000080008081UL, 0x8000000000008009UL,
    0x000000000000008AUL, 0x0000000000000088UL, 0x0000000080008009UL, 0x000000008000000AUL,
    0x000000008000808BUL, 0x800000000000008BUL, 0x8000000000008089UL, 0x8000000000008003UL,
    0x8000000000008002UL, 0x8000000000000080UL, 0x000000000000800AUL, 0x800000008000000AUL,
    0x8000000080008081UL, 0x8000000000008080UL, 0x0000000080000001UL, 0x8000000080008008UL,
};

__constant uint RHO[24] = {1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44};

__constant uint PI[24] = {10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1};

// Keccak-f[1600], 24 rounds. Same structure as tiny-keccak so the CPU and GPU
// implementations stay comparable line for line.
inline void keccakf(ulong *st) {
    ulong bc[5];
    for (int round = 0; round < 24; ++round) {
        // theta
        for (int i = 0; i < 5; ++i) {
            bc[i] = st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20];
        }
        for (int i = 0; i < 5; ++i) {
            ulong t = bc[(i + 4) % 5] ^ ROTL64(bc[(i + 1) % 5], 1);
            for (int j = 0; j < 25; j += 5) {
                st[j + i] ^= t;
            }
        }
        // rho and pi
        ulong t = st[1];
        for (int i = 0; i < 24; ++i) {
            uint j = PI[i];
            ulong tmp = st[j];
            st[j] = ROTL64(t, RHO[i]);
            t = tmp;
        }
        // chi
        for (int j = 0; j < 25; j += 5) {
            for (int i = 0; i < 5; ++i) {
                bc[i] = st[j + i];
            }
            for (int i = 0; i < 5; ++i) {
                st[j + i] ^= (~bc[(i + 1) % 5]) & bc[(i + 2) % 5];
            }
        }
        // iota
        st[0] ^= RC[round];
    }
}

// Leading zero bits of the 32-byte digest, read as a byte stream in order.
inline uint digest_zeros(const ulong *st) {
    uint z = 0;
    for (int l = 0; l < 4; ++l) {
        ulong v = st[l];
        for (int b = 0; b < 8; ++b) {
            uint byte = (uint)((v >> (8 * b)) & 0xffUL);
            if (byte == 0) {
                z += 8;
            } else {
                // clz works on 32 bits, so subtract the 24 bits above the byte.
                return z + clz(byte) - 24;
            }
        }
    }
    return z;
}

// Absorbs one 136-byte block and permutes. The rate is 136 bytes, so lanes
// 0..16 carry data and 17..24 stay zero.
inline void absorb(ulong *st, const uchar *blk) {
    for (int l = 0; l < 25; ++l) {
        st[l] = 0;
    }
    for (int l = 0; l < 17; ++l) {
        ulong v = 0;
        for (int b = 0; b < 8; ++b) {
            v |= ((ulong)blk[l * 8 + b]) << (8 * b);
        }
        st[l] = v;
    }
    keccakf(st);
}

// Builds the fixed parts of the block: prefix, zero nonce, padding byte at 85
// and the 0x80 terminator at the end of the rate.
inline void build_block(uchar *blk, __global const uchar *prefix) {
    for (int i = 0; i < 136; ++i) {
        blk[i] = 0;
    }
    for (int i = 0; i < 53; ++i) {
        blk[i] = prefix[i];
    }
    blk[85] = 0x01;
    blk[135] = 0x80;
}

inline void put_counter(uchar *blk, ulong n) {
    // Big-endian u64 at bytes 77..84, matching the Rust nonce encoding.
    for (int b = 0; b < 8; ++b) {
        blk[77 + b] = (uchar)(n >> (56 - 8 * b));
    }
}

__kernel void grind(__global const uchar *prefix, ulong hi, ulong start, ulong stride, uint iters, uint floor_bits,
                    __global ulong *out_nonce, __global uint *out_zeros, __global uint *out_count, uint max_hits) {
    uint gid = get_global_id(0);

    uchar blk[136];
    build_block(blk, prefix);

    // `hi` owns the top 16 bits of the counter word, which is what keeps this
    // range disjoint from the CPU grinders running at the same time. Each
    // work-item then walks a strided sequence of the low 48 bits, so counters
    // stay unique across the whole launch without any coordination.
    ulong c = start + (ulong)gid;
    ulong step = stride;

    for (uint it = 0; it < iters; ++it) {
        ulong n = hi | (c & MASK48);
        c += step;

        put_counter(blk, n);

        ulong st[25];
        absorb(st, blk);

        uint z = digest_zeros(st);
        if (z >= floor_bits) {
            uint slot = atomic_inc(out_count);
            // Overflow past the buffer is dropped rather than corrupting memory;
            // the host sizes max_hits well above any plausible hit rate.
            if (slot < max_hits) {
                out_nonce[slot] = n;
                out_zeros[slot] = z;
            }
        }
    }
}

// Test kernel: writes the full digest for each supplied counter, so the host can
// compare against the CPU implementation bit for bit.
__kernel void digest_of(__global const uchar *prefix, __global const ulong *counters, __global uchar *out) {
    uint gid = get_global_id(0);

    uchar blk[136];
    build_block(blk, prefix);
    put_counter(blk, counters[gid]);

    ulong st[25];
    absorb(st, blk);

    for (int l = 0; l < 4; ++l) {
        ulong v = st[l];
        for (int b = 0; b < 8; ++b) {
            out[gid * 32 + l * 8 + b] = (uchar)(v >> (8 * b));
        }
    }
}
