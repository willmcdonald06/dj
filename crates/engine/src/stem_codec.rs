//! A small lossless codec for keeping stems compressed in memory and decoding them on
//! the audio thread.
//!
//! The scheme is FLAC's in miniature: 16-bit stereo, cut into blocks of
//! [`BLOCK_FRAMES`] frames; each block is coded as left/right or mid/side, each channel
//! with the best of FLAC's fixed polynomial predictors (orders 0–4), and the residuals
//! Rice-coded in [`PARTITIONS`] partitions with their own parameter. Silent channels
//! (common in stems: vocals between verses) take three bits. Typical stems shrink to
//! 30–50% of their 16-bit size.
//!
//! FLAC itself isn't decoded on the audio thread because its decoders allocate. This
//! one decodes a block into a caller-provided buffer with no allocation and bounded
//! work, and any block can be decoded on its own, so seeking is free.
//!
//! # RT safety
//!
//! [`decode_block`] and [`EncodedStem::block`] are **RT-safe**. Encoding is not.

/// Frames per block. Each block decodes independently.
pub const BLOCK_FRAMES: usize = 1024;
/// Rice partitions per channel per block.
pub const PARTITIONS: usize = 4;
const PARTITION_LEN: usize = BLOCK_FRAMES / PARTITIONS;
const MAX_ORDER: usize = 4;
/// Channel kind code for an all-zero channel.
const KIND_ZERO: u32 = 7;
/// Warm-up samples are stored in this many bits: enough for a side channel (L − R).
const WARMUP_BITS: u32 = 17;
const RICE_PARAM_BITS: u32 = 5;
/// Longest unary run the decoder accepts before treating the data as corrupt.
const MAX_UNARY: u32 = 1 << 22;

/// One stem, compressed. Immutable once built; shared with the audio thread.
#[derive(Debug, Clone)]
pub struct EncodedStem {
    bytes: Box<[u8]>,
    /// Start of each block in `bytes`, plus one past the end.
    offsets: Box<[u32]>,
    frames: u64,
    /// Decoded sample = stored `i16` × `scale`.
    scale: f32,
}

impl EncodedStem {
    /// Frames in the stem.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Compressed size in bytes.
    pub fn compressed_bytes(&self) -> usize {
        self.bytes.len() + self.offsets.len() * 4
    }

    pub fn blocks(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Decode block `index` into `out` (interleaved stereo, at least
    /// `BLOCK_FRAMES * 2` long). Returns the frames decoded (the last block may be
    /// short), or 0 if `index` is out of range.
    ///
    /// RT-safe.
    pub fn block(&self, index: usize, out: &mut [f32]) -> usize {
        let (Some(&start), Some(&end)) = (self.offsets.get(index), self.offsets.get(index + 1))
        else {
            return 0;
        };
        let first = index as u64 * BLOCK_FRAMES as u64;
        let frames = self.frames.saturating_sub(first).min(BLOCK_FRAMES as u64) as usize;
        match self.bytes.get(start as usize..end as usize) {
            Some(bytes) => decode_block(bytes, frames, self.scale, out),
            None => 0,
        }
    }
}

/// Builds an [`EncodedStem`] from audio fed in pieces of any size. Not RT-safe.
pub struct StemEncoder {
    /// Multiplier from `f32` input to stored `i16`.
    quantize: f32,
    scale: f32,
    pending: Vec<i16>,
    bytes: Vec<u8>,
    offsets: Vec<u32>,
    frames: u64,
}

impl StemEncoder {
    /// `gain` is applied on decode: input samples in -1..1 are stored at 16 bits and
    /// come back multiplied by `gain` (stems that peak above full scale are stored
    /// turned down, with the gain to restore them).
    pub fn new(gain: f32) -> Self {
        Self {
            quantize: 32767.0,
            scale: gain / 32767.0,
            pending: Vec::with_capacity(BLOCK_FRAMES * 2),
            bytes: Vec::new(),
            offsets: vec![0],
            frames: 0,
        }
    }

    /// Append planar stereo frames.
    pub fn push(&mut self, left: &[f32], right: &[f32]) {
        for (&l, &r) in left.iter().zip(right) {
            self.pending.push(quantize(l, self.quantize));
            self.pending.push(quantize(r, self.quantize));
            if self.pending.len() == BLOCK_FRAMES * 2 {
                self.flush();
            }
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames + (self.pending.len() / 2) as u64
    }

    pub fn finish(mut self) -> EncodedStem {
        if !self.pending.is_empty() {
            self.flush();
        }
        EncodedStem {
            bytes: self.bytes.into_boxed_slice(),
            offsets: self.offsets.into_boxed_slice(),
            frames: self.frames,
            scale: self.scale,
        }
    }

    fn flush(&mut self) {
        let frames = self.pending.len() / 2;
        encode_block(&self.pending, &mut self.bytes);
        self.offsets.push(self.bytes.len() as u32);
        self.frames += frames as u64;
        self.pending.clear();
    }
}

fn quantize(sample: f32, multiplier: f32) -> i16 {
    (sample * multiplier).round().clamp(-32768.0, 32767.0) as i16
}

// ---------------------------------------------------------------------------------------
// Encoding

fn encode_block(interleaved: &[i16], out: &mut Vec<u8>) {
    let n = interleaved.len() / 2;
    let mut chans = [[0i32; BLOCK_FRAMES]; 4];
    for (i, frame) in interleaved.chunks_exact(2).enumerate() {
        let (l, r) = (frame[0] as i32, frame[1] as i32);
        chans[0][i] = l;
        chans[1][i] = r;
        chans[2][i] = (l + r) >> 1;
        chans[3][i] = l - r;
    }
    let plans = chans.each_ref().map(|c| plan_channel(&c[..n]));
    let left_right = plans[0].cost + plans[1].cost;
    let mid_side = plans[2].cost + plans[3].cost;
    let mut writer = BitWriter::new(out);
    let pick = match mid_side < left_right {
        true => {
            writer.write(1, 1);
            [2, 3]
        }
        false => {
            writer.write(0, 1);
            [0, 1]
        }
    };
    for c in pick {
        write_channel(&mut writer, &chans[c][..n], &plans[c]);
    }
    writer.finish();
}

/// How to code one channel of a block.
struct ChannelPlan {
    /// Predictor order, or `None` for an all-zero channel.
    order: Option<usize>,
    /// Zigzagged residuals for `order` (entries before `order` unused).
    residuals: [u32; BLOCK_FRAMES],
    /// Rice parameter per partition.
    params: [u32; PARTITIONS],
    /// Exact bits.
    cost: u64,
}

fn plan_channel(samples: &[i32]) -> ChannelPlan {
    let mut plan = ChannelPlan {
        order: None,
        residuals: [0; BLOCK_FRAMES],
        params: [0; PARTITIONS],
        cost: 3,
    };
    if samples.iter().all(|&s| s == 0) {
        return plan;
    }
    let n = samples.len();
    // Estimate each order's cost from per-partition sums: a Rice code with
    // k ≈ log2(mean) takes about count × (k + 1) + sum >> k bits.
    let mut best = (0, u64::MAX);
    let mut scratch = [0u32; BLOCK_FRAMES];
    for order in 0..=MAX_ORDER.min(n.saturating_sub(1)) {
        let mut cost = 3 + order as u64 * WARMUP_BITS as u64;
        for (i, slot) in scratch.iter_mut().enumerate().take(n).skip(order) {
            *slot = zigzag(residual(samples, i, order));
        }
        for range in partitions(n, order) {
            let count = range.len() as u64;
            let sum: u64 = scratch[range].iter().map(|&u| u as u64).sum();
            let k = match sum / count {
                0 => 0,
                mean => mean.ilog2(),
            };
            cost += RICE_PARAM_BITS as u64 + count * (k as u64 + 1) + (sum >> k);
        }
        if cost < best.1 {
            best = (order, cost);
            plan.residuals[..n].copy_from_slice(&scratch[..n]);
        }
    }
    let order = best.0;
    plan.order = Some(order);
    plan.cost = 3 + order as u64 * WARMUP_BITS as u64;
    for (p, range) in partitions_indexed(n, order) {
        let (k, bits) = best_rice(&plan.residuals[range]);
        plan.params[p] = k;
        plan.cost += RICE_PARAM_BITS as u64 + bits;
    }
    plan
}

fn write_channel(writer: &mut BitWriter<'_>, samples: &[i32], plan: &ChannelPlan) {
    let Some(order) = plan.order else {
        writer.write(KIND_ZERO, 3);
        return;
    };
    writer.write(order as u32, 3);
    for &s in &samples[..order] {
        writer.write((s as u32) & ((1 << WARMUP_BITS) - 1), WARMUP_BITS);
    }
    for (p, range) in partitions_indexed(samples.len(), order) {
        let k = plan.params[p];
        writer.write(k, RICE_PARAM_BITS);
        for &u in &plan.residuals[range] {
            writer.write_unary(u >> k);
            if k > 0 {
                writer.write(u & ((1u32 << k) - 1), k);
            }
        }
    }
}

/// The Rice parameter with the fewest bits for `residuals`, and that many bits.
fn best_rice(residuals: &[u32]) -> (u32, u64) {
    if residuals.is_empty() {
        return (0, 0);
    }
    let sum: u64 = residuals.iter().map(|&u| u as u64).sum();
    let guess = match sum / residuals.len() as u64 {
        0 => 0,
        m => m.ilog2(),
    };
    let mut best = (0, u64::MAX);
    for k in guess.saturating_sub(1)..=(guess + 1).min(30) {
        let quotients: u64 = residuals.iter().map(|&u| (u >> k) as u64).sum();
        let bits = quotients + residuals.len() as u64 * (k as u64 + 1);
        if bits < best.1 {
            best = (k, bits);
        }
    }
    best
}

/// [`partitions`] with each partition's index (empty partitions are skipped, so the
/// index isn't the position in the iteration).
fn partitions_indexed(
    n: usize,
    order: usize,
) -> impl Iterator<Item = (usize, std::ops::Range<usize>)> {
    (0..PARTITIONS).filter_map(move |p| partition(n, order, p).map(|r| (p, r)))
}

/// Ranges of sample indices covered by each partition's residuals (samples before
/// `order` are warm-up samples, stored verbatim).
fn partitions(n: usize, order: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
    (0..PARTITIONS).filter_map(move |p| partition(n, order, p))
}

fn partition(n: usize, order: usize, p: usize) -> Option<std::ops::Range<usize>> {
    let start = (p * PARTITION_LEN).max(order).min(n);
    let end = ((p + 1) * PARTITION_LEN).min(n);
    match start < end {
        true => Some(start..end),
        false => None,
    }
}

#[inline]
fn predict(x: &[i32], i: usize, order: usize) -> i32 {
    // Wrapping: valid data never overflows, and corrupt data must not panic.
    let at = |back: usize| x[i - back];
    match order {
        0 => 0,
        1 => at(1),
        2 => at(1).wrapping_mul(2).wrapping_sub(at(2)),
        3 => at(1)
            .wrapping_sub(at(2))
            .wrapping_mul(3)
            .wrapping_add(at(3)),
        _ => at(1)
            .wrapping_add(at(3))
            .wrapping_mul(4)
            .wrapping_sub(at(2).wrapping_mul(6))
            .wrapping_sub(at(4)),
    }
}

#[inline]
fn residual(x: &[i32], i: usize, order: usize) -> i32 {
    x[i].wrapping_sub(predict(x, i, order))
}

#[inline]
fn zigzag(r: i32) -> u32 {
    ((r << 1) ^ (r >> 31)) as u32
}

#[inline]
fn unzigzag(u: u32) -> i32 {
    ((u >> 1) as i32) ^ -((u & 1) as i32)
}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    bits: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            out,
            acc: 0,
            bits: 0,
        }
    }

    /// Write the low `count` bits of `value` (count ≤ 32), most significant first.
    fn write(&mut self, value: u32, count: u32) {
        if count == 0 {
            return;
        }
        let value = value as u64 & ((1u64 << count) - 1);
        self.acc = (self.acc << count) | value;
        self.bits += count;
        while self.bits >= 8 {
            self.bits -= 8;
            self.out.push((self.acc >> self.bits) as u8);
        }
    }

    /// `q` zeros then a one.
    fn write_unary(&mut self, mut q: u32) {
        while q >= 32 {
            self.write(0, 32);
            q -= 32;
        }
        self.write(1, q + 1);
    }

    fn finish(mut self) {
        if self.bits > 0 {
            let pad = 8 - self.bits;
            self.write(0, pad);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Decoding

/// Decode one block of `frames` frames from `bytes` into `out` (interleaved stereo),
/// multiplying by `scale`. Returns the frames written; corrupt data decodes to
/// something wrong, never a panic.
///
/// RT-safe: no allocation, work bounded by `frames` (at most [`BLOCK_FRAMES`]).
pub fn decode_block(bytes: &[u8], frames: usize, scale: f32, out: &mut [f32]) -> usize {
    let n = frames.min(BLOCK_FRAMES).min(out.len() / 2);
    let mut reader = BitReader::new(bytes);
    let mid_side = reader.read(1) == 1;
    let mut chans = [[0i32; BLOCK_FRAMES]; 2];
    for chan in &mut chans {
        decode_channel(&mut reader, &mut chan[..n]);
    }
    let [a, b] = &chans;
    for (i, frame) in out[..n * 2].chunks_exact_mut(2).enumerate() {
        let (l, r) = match mid_side {
            true => {
                let side = b[i];
                let mid = (a[i] << 1) | (side & 1);
                (mid.wrapping_add(side) >> 1, mid.wrapping_sub(side) >> 1)
            }
            false => (a[i], b[i]),
        };
        frame[0] = l as f32 * scale;
        frame[1] = r as f32 * scale;
    }
    n
}

fn decode_channel(reader: &mut BitReader<'_>, x: &mut [i32]) {
    let kind = reader.read(3);
    if kind == KIND_ZERO || kind as usize > MAX_ORDER {
        x.fill(0);
        return;
    }
    let order = (kind as usize).min(x.len());
    for s in &mut x[..order] {
        // Sign-extend the 17-bit warm-up sample.
        let raw = reader.read(WARMUP_BITS);
        *s = ((raw << (32 - WARMUP_BITS)) as i32) >> (32 - WARMUP_BITS);
    }
    for range in partitions(x.len(), order) {
        let k = reader.read(RICE_PARAM_BITS);
        for i in range {
            let q = reader.read_unary();
            let u = (q << k) | reader.read(k);
            x[i] = predict(x, i, order).wrapping_add(unzigzag(u));
        }
    }
}

struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// Unread bits, left-aligned.
    acc: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            acc: 0,
            bits: 0,
        }
    }

    #[inline]
    fn refill(&mut self) {
        while self.bits <= 56 {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return;
            };
            self.acc |= (byte as u64) << (56 - self.bits);
            self.pos += 1;
            self.bits += 8;
        }
    }

    /// Read `count` bits (≤ 32). Past the end of the data, reads zeros.
    #[inline]
    fn read(&mut self, count: u32) -> u32 {
        if count == 0 {
            return 0;
        }
        self.refill();
        let value = (self.acc >> (64 - count)) as u32;
        self.consume(count);
        value
    }

    #[inline]
    fn consume(&mut self, count: u32) {
        let count = count.min(self.bits);
        self.acc = match count {
            64 => 0,
            c => self.acc << c,
        };
        self.bits -= count;
    }

    /// Count zeros up to and including the next one bit.
    #[inline]
    fn read_unary(&mut self) -> u32 {
        let mut q = 0;
        // Each pass consumes at least a byte's worth of bits or finishes; MAX_UNARY
        // bounds it on corrupt data.
        while q < MAX_UNARY {
            self.refill();
            if self.bits == 0 {
                return q;
            }
            let zeros = self.acc.leading_zeros().min(self.bits);
            if zeros < self.bits {
                self.consume(zeros + 1);
                return q + zeros;
            }
            q += zeros;
            self.consume(zeros);
        }
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(left: &[f32], right: &[f32]) -> (EncodedStem, Vec<f32>) {
        let mut encoder = StemEncoder::new(1.0);
        // Feed in odd-sized pieces to exercise the block buffering.
        for (l, r) in left.chunks(777).zip(right.chunks(777)) {
            encoder.push(l, r);
        }
        let stem = encoder.finish();
        let mut decoded = Vec::new();
        let mut block = vec![0.0; BLOCK_FRAMES * 2];
        for b in 0..stem.blocks() {
            let n = stem.block(b, &mut block);
            decoded.extend_from_slice(&block[..n * 2]);
        }
        (stem, decoded)
    }

    fn assert_lossless(left: &[f32], right: &[f32]) -> EncodedStem {
        let (stem, decoded) = roundtrip(left, right);
        assert_eq!(stem.frames(), left.len() as u64);
        assert_eq!(decoded.len(), left.len() * 2);
        for (i, frame) in decoded.chunks_exact(2).enumerate() {
            let want_l = quantize(left[i], 32767.0) as f32 * (1.0 / 32767.0);
            let want_r = quantize(right[i], 32767.0) as f32 * (1.0 / 32767.0);
            assert_eq!(frame[0], want_l, "left frame {i}");
            assert_eq!(frame[1], want_r, "right frame {i}");
        }
        stem
    }

    /// Deterministic noise in -1..1.
    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / (1u32 << 23) as f32 - 1.0
            })
            .collect()
    }

    #[test]
    fn music_like_signal_roundtrips_and_compresses() {
        let n = BLOCK_FRAMES * 10 + 123;
        let left: Vec<f32> = (0..n)
            .map(|i| 0.5 * (i as f32 * 0.01).sin() + 0.2 * (i as f32 * 0.13).sin())
            .collect();
        let right: Vec<f32> = left.iter().map(|x| x * 0.9).collect();
        let stem = assert_lossless(&left, &right);
        let raw = n * 4;
        assert!(
            stem.compressed_bytes() < raw / 2,
            "{} of {raw}",
            stem.compressed_bytes()
        );
    }

    #[test]
    fn noise_extremes_and_silence_roundtrip() {
        let n = BLOCK_FRAMES * 3;
        assert_lossless(&noise(n, 1), &noise(n, 2));
        let square: Vec<f32> = (0..n)
            .map(|i| match (i / 7) % 2 {
                0 => 1.0,
                _ => -1.0,
            })
            .collect();
        let inverted: Vec<f32> = square.iter().map(|x| -x).collect();
        assert_lossless(&square, &inverted);
        let silent = vec![0.0; n];
        let stem = assert_lossless(&silent, &silent);
        assert!(stem.compressed_bytes() < 100);
    }

    #[test]
    fn tiny_and_partial_blocks_roundtrip() {
        for n in [
            1,
            2,
            3,
            5,
            255,
            256,
            257,
            BLOCK_FRAMES - 1,
            BLOCK_FRAMES + 1,
        ] {
            assert_lossless(&noise(n, n as u32), &noise(n, 7));
        }
    }

    #[test]
    fn gain_scales_on_decode() {
        let mut encoder = StemEncoder::new(2.0);
        encoder.push(&[0.5, -0.25], &[1.0, 0.0]);
        let stem = encoder.finish();
        let mut out = vec![0.0; BLOCK_FRAMES * 2];
        assert_eq!(stem.block(0, &mut out), 2);
        assert!((out[0] - 1.0).abs() < 1e-4);
        assert!((out[1] - 2.0).abs() < 1e-4);
        assert!((out[2] + 0.5).abs() < 1e-4);
        assert_eq!(stem.block(1, &mut out), 0);
    }

    #[test]
    fn corrupt_data_does_not_panic() {
        let mut out = vec![0.0; BLOCK_FRAMES * 2];
        for seed in 0..50 {
            let bytes: Vec<u8> = noise(64, seed)
                .iter()
                .map(|x| (x * 127.0) as i8 as u8)
                .collect();
            decode_block(&bytes, BLOCK_FRAMES, 1.0, &mut out);
        }
        decode_block(&[], BLOCK_FRAMES, 1.0, &mut out);
        decode_block(&[0; 4], BLOCK_FRAMES, 1.0, &mut out);
    }
}
