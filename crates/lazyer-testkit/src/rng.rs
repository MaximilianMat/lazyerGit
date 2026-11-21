//! Small deterministic generator (SplitMix64) so fixtures are identical on
//! every machine and toolchain.

#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Uniform in `[0, 1)`.
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }

    pub fn normal(&mut self) -> f64 {
        let u1 = self.f64().max(1e-12);
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    pub fn lognormal(&mut self, median: f64, sigma: f64) -> f64 {
        median * (sigma * self.normal()).exp()
    }

    /// `1 + geometric`, with the given mean (`mean >= 1`).
    pub fn count_with_mean(&mut self, mean: f64) -> usize {
        let p = 1.0 / mean.max(1.0);
        let mut n = 1;
        while !self.chance(p) && n < 10_000 {
            n += 1;
        }
        n
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

pub fn mix(a: u64, b: u64, c: u64) -> u64 {
    let mut r = Rng::new(a.wrapping_mul(0x100_0000_01B3) ^ b.rotate_left(21) ^ c.rotate_left(42));
    r.next_u64()
}
