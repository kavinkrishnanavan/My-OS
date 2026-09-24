//! Randomness for the TLS handshake (nonces, the ephemeral ECDHE key,
//! etc.) — `rand_core` 0.6, the exact major version `embedded-tls` 0.19
//! pins, so its `RngCore`/`CryptoRng` traits are the same types.
//!
//! Prefers `RDRAND` (a real hardware entropy source, present on every
//! x86_64 CPU since ~2012 and passed through by QEMU/KVM) and only falls
//! back to a seeded xorshift64 PRNG — clearly **not** cryptographically
//! secure — if `RDRAND` isn't available. The fallback exists so TLS
//! still *works* on hardware/emulators without it; it does not make that
//! session's handshake randomness actually secure. See the `CryptoRng`
//! impl below and the README's security notes.

use core::arch::x86_64::{__cpuid, _rdrand64_step, _rdtsc};
use core::sync::atomic::{AtomicU64, Ordering};
use rand_core::{impls, CryptoRng, Error, RngCore};
use spin::Once;

static HAS_RDRAND: Once<bool> = Once::new();
static FALLBACK_STATE: AtomicU64 = AtomicU64::new(0);

fn has_rdrand() -> bool {
    *HAS_RDRAND.call_once(|| {
        // CPUID leaf 1, ECX bit 30. Always safe to call: CPUID itself
        // needs no target-feature gate.
        let regs = __cpuid(1);
        regs.ecx & (1 << 30) != 0
    })
}

#[target_feature(enable = "rdrand")]
unsafe fn rdrand64() -> Option<u64> {
    let mut val: u64 = 0;
    // Intel's guidance: retry a bounded number of times before treating
    // the source as (transiently) exhausted.
    for _ in 0..16 {
        if _rdrand64_step(&mut val) == 1 {
            return Some(val);
        }
    }
    None
}

fn fallback_next_u64() -> u64 {
    // xorshift64*, seeded from the timestamp counter mixed with a
    // monotonic counter so repeated calls in the same tick still differ.
    // This is fast, deterministic-if-you-know-the-seed randomness, not
    // real entropy — see the module doc comment.
    let mut x = FALLBACK_STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = unsafe { _rdtsc() } ^ 0x9E3779B97F4A7C15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    FALLBACK_STATE.store(x, Ordering::Relaxed);
    x.wrapping_mul(0x2545F4914F6CDD1D)
}

pub struct Rng {
    /// Set the first time the fallback path is used, so callers (the TLS
    /// setup code) can log a one-time warning instead of staying silent
    /// about weakened randomness.
    pub used_fallback: bool,
}

impl Rng {
    pub fn new() -> Self {
        Rng {
            used_fallback: !has_rdrand(),
        }
    }

    fn next_raw_u64(&mut self) -> u64 {
        if has_rdrand() {
            if let Some(v) = unsafe { rdrand64() } {
                return v;
            }
            self.used_fallback = true; // RDRAND transiently exhausted
        }
        fallback_next_u64()
    }
}

impl RngCore for Rng {
    fn next_u32(&mut self) -> u32 {
        self.next_raw_u64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        self.next_raw_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        impls::fill_bytes_via_next(self, dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

/// # Security
/// Only actually a cryptographically secure RNG when `RDRAND` is
/// available (`used_fallback == false` after construction) — the xorshift
/// fallback is not. `embedded-tls` requires this marker trait to accept
/// an RNG at all, so it's implemented unconditionally; callers that care
/// should check `Rng::used_fallback` and treat a `true` value as "this
/// session's TLS handshake randomness is weak."
impl CryptoRng for Rng {}
