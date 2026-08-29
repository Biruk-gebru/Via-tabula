// To make a bloom filter we have to have two values, p and n: p being the rate of false
// positives we desire, and n being the amount of keys we expect.
// The formulas are:
//   m = -n * ln(p) / (ln 2)^2   (bit array size)
//   k = (m / n) * ln 2          (number of hash functions)
// So for p = 1% and n = 10000 we get m = 95,851 and k = 7, rounding both to the nearest
// integer since bit array size and hash function count must be whole numbers.
// We only implement 2 real hash functions (FNV-1a and djb2), not k of them. To still get
// k distinct bit positions per key, we combine the two via double hashing:
// h_i(key) = h1(key) + i * h2(key), for i in 0..k. k itself stays the computed value above
// (7), not 2 — "2 hash functions" refers to the implementation primitives, not k.

pub struct BloomFilter {
    bits: Vec<u8>,
    k: usize,
    m: usize,
}

impl BloomFilter {
    pub fn new(expected_keys: usize, fp_rate: f64) -> Self {
        let n = expected_keys as f64;
        let p = fp_rate;

        let m = (-n * p.ln() / std::f64::consts::LN_2.powi(2)).ceil() as usize;
        let k = ((m as f64 / n) * std::f64::consts::LN_2).round() as usize;

        // m is a bit count; dividing by 8 converts it to bytes, but plain integer division
        // truncates (e.g. 95851/8 = 11981.375 -> 11981, one byte short). Adding 7 first
        // forces any nonzero remainder to push the result up by one, giving ceil(m / 8).
        let byte_len = (m + 7) / 8;

        BloomFilter {
            bits: vec![0u8; byte_len],
            k,
            m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_computes_m_and_k_matching_hand_calculation() {
        let filter = BloomFilter::new(10_000, 0.01);

        // hand-calculated in the comment above: m = 95,851, k = 7
        assert_eq!(filter.m, 95_851);
        assert_eq!(filter.k, 7);

        // bits vec must be large enough to hold m bits, rounded up to a whole byte
        assert_eq!(filter.bits.len(), (filter.m + 7) / 8);
        assert!(filter.bits.iter().all(|&byte| byte == 0));
    }
}
