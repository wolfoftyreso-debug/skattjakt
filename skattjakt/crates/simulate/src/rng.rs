//! The random source.
//!
//! Written here rather than taken from a crate, and the reason is
//! reproducibility rather than pride. Section 12 requires that the same seed,
//! the same inputs and the same engine version reproduce a result. A generator
//! from a dependency makes that promise depend on a version range: `rand`
//! changed its `StdRng` algorithm between major versions, and a project that
//! had persisted seeds would have found its old runs unreproducible after a
//! routine `cargo update`. The sequence a seed produces is part of this
//! system's contract, so it lives in this system's source, pinned by test
//! vectors that fail if a byte of it ever changes.
//!
//! Two generators, both standard and both small:
//!
//! - `SplitMix64` — used only to expand a seed into the state of the other. It
//!   is what turns a seed of `1` into well-distributed state instead of a
//!   generator that starts in a corner of its space.
//! - `Xoshiro256PlusPlus` — the working generator. Period 2^256 − 1, passes
//!   BigCrush, and produces a `u64` in a handful of instructions, which matters
//!   when a run draws twelve million of them.
//!
//! Neither is cryptographically secure and neither is used where that matters.
//! Session tokens come from `getrandom`; see `skattjakt-identity`.

/// Expands a seed into generator state.
///
/// Also used on its own to derive one stream's seed from another, which is how
/// each input variable gets an independent sequence.
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The working generator.
#[derive(Debug, Clone)]
pub struct Rng {
    state: [u64; 4],
    /// Kept for the second normal deviate of a Box–Muller pair, so half the
    /// pairs are not thrown away. `None` when the cache is empty.
    spare_normal: Option<f64>,
}

impl Rng {
    /// Seeds a generator.
    ///
    /// The seed is expanded through `SplitMix64` because xoshiro is documented
    /// to require it: seeded directly with a small integer it produces
    /// low-quality output for the first several draws, which in a Monte Carlo
    /// run is not a curiosity but a bias in the first iterations.
    pub fn new(seed: u64) -> Self {
        let mut mix = SplitMix64::new(seed);
        Self {
            state: [
                mix.next_u64(),
                mix.next_u64(),
                mix.next_u64(),
                mix.next_u64(),
            ],
            spare_normal: None,
        }
    }

    /// A generator for one named stream, derived from the run's seed.
    ///
    /// Every input variable draws from its own stream. This is what makes a
    /// simulation reproducible under editing: adding a thirteenth input, or
    /// reordering the list, does not shift the numbers the other twelve see.
    /// A single shared stream would make every historical run irreproducible
    /// the moment someone inserted a variable.
    pub fn for_stream(seed: u64, stream: &str) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
        for byte in stream.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        Self::new(seed ^ hash)
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let result = self.state[0]
            .wrapping_add(self.state[3])
            .rotate_left(23)
            .wrapping_add(self.state[0]);
        let t = self.state[1] << 17;
        self.state[2] ^= self.state[0];
        self.state[3] ^= self.state[1];
        self.state[1] ^= self.state[2];
        self.state[0] ^= self.state[3];
        self.state[2] ^= t;
        self.state[3] = self.state[3].rotate_left(45);
        result
    }

    /// A uniform in `[0, 1)`.
    ///
    /// Built from the top 53 bits, which is every bit an `f64` mantissa can
    /// hold. Taking the low bits instead is the classic error: the low bits of
    /// many generators are the weakest, and dividing a full `u64` by `2^64`
    /// loses to rounding at the top of the range.
    #[inline]
    pub fn uniform01(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0) // 2^53
    }

    /// A uniform in `(0, 1)` — never exactly zero.
    ///
    /// Needed wherever a logarithm follows: `ln(0)` is `-inf`, and an
    /// exponential or lognormal draw that hits it turns one iteration into an
    /// infinity that then propagates through every statistic downstream.
    #[inline]
    pub fn open_uniform01(&mut self) -> f64 {
        loop {
            let u = self.uniform01();
            if u > 0.0 {
                return u;
            }
        }
    }

    #[inline]
    pub fn uniform(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.uniform01()
    }

    /// A standard normal deviate, by the polar form of Box–Muller.
    ///
    /// The polar form rather than the trigonometric one: it needs no `sin` or
    /// `cos`, and rejection costs less than two transcendental calls. Both
    /// deviates of a pair are used.
    pub fn standard_normal(&mut self) -> f64 {
        if let Some(spare) = self.spare_normal.take() {
            return spare;
        }
        loop {
            let u = 2.0 * self.uniform01() - 1.0;
            let v = 2.0 * self.uniform01() - 1.0;
            let s = u * u + v * v;
            if s > 0.0 && s < 1.0 {
                let factor = (-2.0 * s.ln() / s).sqrt();
                self.spare_normal = Some(v * factor);
                return u * factor;
            }
        }
    }

    /// A gamma deviate, shape `k > 0`, scale 1.
    ///
    /// Marsaglia–Tsang. Used by the beta distribution, which is a ratio of two
    /// gammas — the alternative for beta is inverting an incomplete beta
    /// function numerically per draw, which is both slower and less accurate.
    pub fn standard_gamma(&mut self, shape: f64) -> f64 {
        // Marsaglia–Tsang needs shape >= 1; below that the standard boost
        // relation gamma(k) = gamma(k+1) * U^(1/k) applies.
        if shape < 1.0 {
            let boosted = self.standard_gamma(shape + 1.0);
            return boosted * self.open_uniform01().powf(1.0 / shape);
        }
        let d = shape - 1.0 / 3.0;
        let c = 1.0 / (9.0 * d).sqrt();
        loop {
            let x = self.standard_normal();
            let v = 1.0 + c * x;
            if v <= 0.0 {
                continue;
            }
            let v = v * v * v;
            let u = self.open_uniform01();
            if u < 1.0 - 0.0331 * x * x * x * x {
                return d * v;
            }
            if u.ln() < 0.5 * x * x + d * (1.0 - v + v.ln()) {
                return d * v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the whole module. If this test ever has to be updated,
    /// every persisted seed in the database has become meaningless, and that
    /// must be a deliberate act with a version bump — not a silent consequence
    /// of an edit.
    #[test]
    fn the_sequence_for_a_seed_is_pinned() {
        let mut rng = Rng::new(42);
        let drawn: Vec<u64> = (0..5).map(|_| rng.next_u64()).collect();
        assert_eq!(
            drawn,
            vec![
                15021278609987233951,
                5881210131331364753,
                18149643915985481100,
                12933668939759105464,
                14637574242682825331,
            ],
            "the generator's output changed; every stored seed is now unreproducible"
        );
    }

    #[test]
    fn the_same_seed_gives_the_same_run() {
        let a: Vec<f64> = {
            let mut rng = Rng::new(7);
            (0..1000).map(|_| rng.uniform01()).collect()
        };
        let b: Vec<f64> = {
            let mut rng = Rng::new(7);
            (0..1000).map(|_| rng.uniform01()).collect()
        };
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_give_different_runs() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let differing = (0..100).filter(|_| a.uniform01() != b.uniform01()).count();
        assert!(differing > 95, "two seeds produced near-identical output");
    }

    #[test]
    fn streams_are_independent_of_each_other() {
        // The property that makes a simulation editable: the numbers "customers"
        // sees do not depend on whether "costs" exists.
        let a: Vec<f64> = {
            let mut rng = Rng::for_stream(99, "customers");
            (0..100).map(|_| rng.uniform01()).collect()
        };
        let b: Vec<f64> = {
            let mut other = Rng::for_stream(99, "costs");
            for _ in 0..50 {
                other.uniform01();
            }
            let mut rng = Rng::for_stream(99, "customers");
            (0..100).map(|_| rng.uniform01()).collect()
        };
        assert_eq!(a, b);
    }

    #[test]
    fn uniforms_stay_inside_the_unit_interval() {
        let mut rng = Rng::new(3);
        for _ in 0..200_000 {
            let u = rng.uniform01();
            assert!((0.0..1.0).contains(&u), "produced {u}");
        }
    }

    #[test]
    fn the_open_uniform_never_returns_zero() {
        let mut rng = Rng::new(4);
        for _ in 0..200_000 {
            assert!(rng.open_uniform01() > 0.0);
        }
    }

    #[test]
    fn uniforms_are_roughly_flat() {
        // Ten buckets, 100_000 draws: each bucket should hold about 10_000.
        // A generator with a stuck bit or a bad seeding step fails this loudly.
        let mut rng = Rng::new(11);
        let mut buckets = [0usize; 10];
        for _ in 0..100_000 {
            buckets[(rng.uniform01() * 10.0) as usize] += 1;
        }
        for (index, count) in buckets.iter().enumerate() {
            assert!(
                (9_400..10_600).contains(count),
                "bucket {index} held {count}, which is not flat"
            );
        }
    }

    #[test]
    fn standard_normals_have_the_right_moments() {
        let mut rng = Rng::new(5);
        let n = 200_000;
        let samples: Vec<f64> = (0..n).map(|_| rng.standard_normal()).collect();
        let mean = samples.iter().sum::<f64>() / n as f64;
        let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.02, "mean was {mean}");
        assert!((variance - 1.0).abs() < 0.02, "variance was {variance}");
    }

    #[test]
    fn gamma_has_the_right_mean_for_shapes_either_side_of_one() {
        for shape in [0.4_f64, 1.0, 3.5, 12.0] {
            let mut rng = Rng::new(13);
            let n = 100_000;
            let mean: f64 = (0..n).map(|_| rng.standard_gamma(shape)).sum::<f64>() / n as f64;
            let tolerance = 0.05 * shape.max(1.0);
            assert!(
                (mean - shape).abs() < tolerance,
                "shape {shape} gave mean {mean}"
            );
        }
    }
}
