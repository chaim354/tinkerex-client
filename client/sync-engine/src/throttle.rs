//! Client-side bandwidth throttling for chunk transfer (parity-roadmap #6 /
//! research-sync-competitors §4 #6 — *client-only, no protocol change*).
//!
//! [`ThrottledChunkIO`] is a **decorator over any [`ChunkIO`]** (the same pattern as the
//! fault-injection harness's `FaultyChunkIO`): it token-bucket rate-limits the bytes it
//! passes through to the inner transport on `put` (upload) and `get` (download). Separate
//! up/down caps; either side `None` (or 0) = unlimited = a transparent pass-through.
//!
//! The limiter is a classic **token bucket**: tokens (bytes) refill continuously at the
//! configured rate up to a burst-sized cap; a transfer that needs more tokens than are on
//! hand sleeps until they've accrued, then proceeds. This smooths throughput to ~`rate`
//! bytes/sec over time while permitting short bursts (the chunk granularity ≈ FastCDC's
//! 1–4 MB), exactly like Dropbox/rclone-style client throttles.
//!
//! No new dependencies — pure `std` (`Mutex` + `Instant` + `thread::sleep`). A single
//! [`RateLimiter`] is shared (`Arc`) across every transport instance the daemon constructs
//! per tick, so the cap is **global per direction**, not per-call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{ChunkIO, Result};

/// A token-bucket rate limiter measured in **bytes per second**. Shareable via [`Arc`]:
/// all transports that clone the same `Arc<RateLimiter>` draw from one bucket, so the
/// effective cap is global for that direction.
///
/// `rate == 0` means *unlimited* — every [`consume`](RateLimiter::consume) returns
/// immediately. A live cap blocks the caller (sleeps) until `n` bytes' worth of tokens
/// have accrued.
#[derive(Debug)]
pub struct RateLimiter {
    rate: f64, // bytes/sec; 0.0 = unlimited
    burst: f64, // max tokens (bytes) the bucket holds
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// A limiter capped at `bytes_per_sec`. `burst_bytes` is the bucket capacity (max
    /// tokens); via [`new`](Self::new) it defaults to one second's worth of bytes, which
    /// lets a single large chunk go through without being sliced while still averaging to
    /// `rate`.
    pub fn with_burst(bytes_per_sec: u64, burst_bytes: u64) -> Self {
        let rate = bytes_per_sec as f64;
        // A zero/too-small burst would stall any transfer larger than the burst forever; floor
        // it at the rate (1s) so a single chunk always eventually fits.
        let burst = (burst_bytes as f64).max(rate);
        Self {
            rate,
            burst,
            state: Mutex::new(Bucket { tokens: burst, last: Instant::now() }),
        }
    }

    /// A limiter capped at `bytes_per_sec`, bucket = 1 second of bytes (sensible default).
    pub fn new(bytes_per_sec: u64) -> Self {
        Self::with_burst(bytes_per_sec, bytes_per_sec)
    }

    /// Build an `Arc<RateLimiter>` from an optional cap. `None` or `Some(0)` → unlimited.
    pub fn from_opt(bytes_per_sec: Option<u64>) -> Arc<Self> {
        Arc::new(Self::new(bytes_per_sec.unwrap_or(0)))
    }

    /// True if this limiter imposes no cap (rate 0) — used to skip all bookkeeping.
    pub fn is_unlimited(&self) -> bool {
        self.rate <= 0.0
    }

    /// Block until `n` bytes' worth of tokens are available, then consume them. Returns
    /// immediately when unlimited. The wait is computed under the lock but **slept outside
    /// it** so concurrent transfers don't serialize on the mutex while sleeping.
    pub fn consume(&self, n: usize) {
        if self.is_unlimited() || n == 0 {
            return;
        }
        let need = n as f64;
        loop {
            let sleep_for = {
                let mut b = self.state.lock().unwrap_or_else(|e| e.into_inner());
                self.refill(&mut b);
                if b.tokens >= need {
                    b.tokens -= need;
                    return;
                }
                // A single request may exceed the burst cap: clamp the outstanding need to the
                // burst so the bucket can actually satisfy it, and wait for that to accrue.
                let deficit = (need - b.tokens).min(self.burst);
                Duration::from_secs_f64(deficit / self.rate)
            };
            // Sleep outside the lock, then loop to re-check (another caller may have drained
            // the bucket meanwhile — the re-check keeps the cap correct under contention).
            std::thread::sleep(sleep_for);
        }
    }

    fn refill(&self, b: &mut Bucket) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        if elapsed > 0.0 {
            b.tokens = (b.tokens + elapsed * self.rate).min(self.burst);
            b.last = now;
        }
    }
}

/// Wraps any [`ChunkIO`], token-bucket rate-limiting the bytes it transfers. Upload
/// (`put`) bytes are gated by `up`; download (`get`) bytes by `down`. Each side `None` =
/// that direction is unlimited (a transparent pass-through).
pub struct ThrottledChunkIO<I: ChunkIO> {
    inner: I,
    up: Option<Arc<RateLimiter>>,
    down: Option<Arc<RateLimiter>>,
}

impl<I: ChunkIO> ThrottledChunkIO<I> {
    /// Wrap `inner` with optional up/down limiters. `None` on a side = unlimited there.
    pub fn new(inner: I, up: Option<Arc<RateLimiter>>, down: Option<Arc<RateLimiter>>) -> Self {
        // Treat an unlimited limiter as absent so the hot path skips it entirely.
        let up = up.filter(|l| !l.is_unlimited());
        let down = down.filter(|l| !l.is_unlimited());
        Self { inner, up, down }
    }

    /// Convenience: build directly from optional byte/sec caps (`None`/`0` = unlimited).
    pub fn from_caps(inner: I, up_bps: Option<u64>, down_bps: Option<u64>) -> Self {
        let up = up_bps.filter(|&b| b > 0).map(|b| Arc::new(RateLimiter::new(b)));
        let down = down_bps.filter(|&b| b > 0).map(|b| Arc::new(RateLimiter::new(b)));
        Self::new(inner, up, down)
    }

    /// The wrapped transport.
    pub fn inner(&self) -> &I {
        &self.inner
    }
}

impl<I: ChunkIO> ChunkIO for ThrottledChunkIO<I> {
    fn put(&self, hash: &str, put_urls: &HashMap<String, String>, bytes: Vec<u8>) -> Result<()> {
        if let Some(l) = &self.up {
            l.consume(bytes.len());
        }
        self.inner.put(hash, put_urls, bytes)
    }

    fn get(&self, hash: &str, get_url: Option<&str>) -> Result<Vec<u8>> {
        let bytes = self.inner.get(hash, get_url)?;
        // Throttle on the way out (after the fetch): we don't know the size until we have it,
        // and gating post-fetch still bounds the *sustained* download rate to the cap.
        if let Some(l) = &self.down {
            l.consume(bytes.len());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An in-memory fake `ChunkIO`: `put` records bytes, `get` returns a fixed blob. Zero
    /// real I/O so the test measures *only* the throttle's sleep.
    struct FakeIO {
        blob: Vec<u8>,
        puts: AtomicUsize,
        gets: AtomicUsize,
    }
    impl FakeIO {
        fn new(blob_len: usize) -> Self {
            Self { blob: vec![0u8; blob_len], puts: AtomicUsize::new(0), gets: AtomicUsize::new(0) }
        }
    }
    impl ChunkIO for FakeIO {
        fn put(&self, _h: &str, _u: &HashMap<String, String>, bytes: Vec<u8>) -> Result<()> {
            self.puts.fetch_add(bytes.len(), Ordering::Relaxed);
            Ok(())
        }
        fn get(&self, _h: &str, _u: Option<&str>) -> Result<Vec<u8>> {
            self.gets.fetch_add(self.blob.len(), Ordering::Relaxed);
            Ok(self.blob.clone())
        }
    }

    fn no_urls() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn unlimited_is_a_passthrough_noop() {
        // None on both sides: transfers must not sleep and must deliver the inner result.
        let io = ThrottledChunkIO::new(FakeIO::new(1 << 20), None, None);
        let start = Instant::now();
        for _ in 0..16 {
            io.put("h", &no_urls(), vec![0u8; 1 << 20]).unwrap();
            let got = io.get("h", None).unwrap();
            assert_eq!(got.len(), 1 << 20);
        }
        // 32 MB through with no cap should be effectively instant.
        assert!(start.elapsed() < Duration::from_millis(200), "unlimited path slept: {:?}", start.elapsed());
        assert_eq!(io.inner().puts.load(Ordering::Relaxed), 16 << 20);
        assert_eq!(io.inner().gets.load(Ordering::Relaxed), 16 << 20);
    }

    #[test]
    fn zero_cap_means_unlimited() {
        // from_caps with 0 / None must behave exactly like the no-op path.
        let io = ThrottledChunkIO::from_caps(FakeIO::new(1 << 20), Some(0), None);
        let start = Instant::now();
        for _ in 0..8 {
            io.put("h", &no_urls(), vec![0u8; 1 << 20]).unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(200), "0-cap slept: {:?}", start.elapsed());
    }

    #[test]
    fn upload_is_rate_limited() {
        // 1 MB/s cap, push 4 MB → must take ~ (4 MB - burst)/rate. Burst defaults to 1s
        // (= 1 MB), so the first ~1 MB is free and the remaining ~3 MB takes ~3 s.
        let rate = 1_000_000u64;
        let io = ThrottledChunkIO::from_caps(FakeIO::new(0), Some(rate), None);
        let total = 4_000_000usize;
        let chunk = 500_000usize;
        let start = Instant::now();
        for _ in 0..(total / chunk) {
            io.put("h", &no_urls(), vec![0u8; chunk]).unwrap();
        }
        let elapsed = start.elapsed();
        // Expected lower bound: (total - burst) / rate. burst == rate (1s default).
        let expected = Duration::from_secs_f64((total as f64 - rate as f64) / rate as f64);
        assert!(
            elapsed >= expected.mul_f64(0.85),
            "upload finished too fast: {elapsed:?} < {expected:?} (cap not enforced)"
        );
        // And it shouldn't take absurdly longer than ~total/rate either.
        assert!(elapsed < Duration::from_secs_f64(total as f64 / rate as f64 + 2.0), "too slow: {elapsed:?}");
        assert_eq!(io.inner().puts.load(Ordering::Relaxed), total);
    }

    #[test]
    fn download_is_rate_limited() {
        // 1 MB/s down cap, fetch 4 × 1 MB chunks → ~3 s after the 1 MB burst.
        let rate = 1_000_000u64;
        let blob = 1_000_000usize;
        let io = ThrottledChunkIO::from_caps(FakeIO::new(blob), None, Some(rate));
        let n = 4;
        let start = Instant::now();
        for _ in 0..n {
            let got = io.get("h", None).unwrap();
            assert_eq!(got.len(), blob);
        }
        let elapsed = start.elapsed();
        let total = (blob * n) as f64;
        let expected = Duration::from_secs_f64((total - rate as f64) / rate as f64);
        assert!(
            elapsed >= expected.mul_f64(0.85),
            "download finished too fast: {elapsed:?} < {expected:?} (cap not enforced)"
        );
    }

    #[test]
    fn shared_limiter_caps_across_two_wrappers() {
        // One Arc<RateLimiter> shared by two ThrottledChunkIO instances → the cap is global,
        // not per-wrapper. Pushing through both must still average to ~rate.
        let rate = 1_000_000u64;
        let limiter = Arc::new(RateLimiter::new(rate));
        let io_a = ThrottledChunkIO::new(FakeIO::new(0), Some(limiter.clone()), None);
        let io_b = ThrottledChunkIO::new(FakeIO::new(0), Some(limiter.clone()), None);
        let chunk = 500_000usize;
        let start = Instant::now();
        // 4 MB total alternating between the two wrappers.
        for i in 0..8 {
            let io = if i % 2 == 0 { &io_a } else { &io_b };
            io.put("h", &no_urls(), vec![0u8; chunk]).unwrap();
        }
        let elapsed = start.elapsed();
        let total = 8.0 * chunk as f64;
        let expected = Duration::from_secs_f64((total - rate as f64) / rate as f64);
        assert!(
            elapsed >= expected.mul_f64(0.85),
            "shared limiter didn't cap globally: {elapsed:?} < {expected:?}"
        );
    }
}
