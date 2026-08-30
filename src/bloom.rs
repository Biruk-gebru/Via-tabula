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
        // Replaced the manual (m + 7) / 8 with m.div_ceil(8) for robustness: it can't
        // silently break if the divisor ever changes without the +7 being updated to match.
        let byte_len = m.div_ceil(8);

        BloomFilter {
            bits: vec![0u8; byte_len],
            k,
            m,
        }
    }

    fn fnv_1a(key: &[u8]) -> u64 {
        let offset_basis: u64 = 14695981039346656037;
        let prime: u64 = 1099511628211;

        let mut hash = offset_basis;

        for &byte in key {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(prime);
        }

        hash
    }

    fn djb2(key: &[u8]) -> u64 {
        let mut hash: u64 = 5381;
        for &byte in key {
            hash = (hash << 5).wrapping_add(hash).wrapping_add(byte as u64);
        }

        hash
    }

    pub fn insert(&mut self, key: &[u8]) {
        let h1 = Self::fnv_1a(key);
        let h2 = Self::djb2(key);

        for i in 0..self.k {
            let combined = h1.wrapping_add((i as u64).wrapping_mul(h2));
            let pos = (combined % self.m as u64) as usize;

            let byte_index = pos / 8;
            let bit_index = pos % 8;
            self.bits[byte_index] |= 1 << bit_index;
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
        assert_eq!(filter.bits.len(), filter.m.div_ceil(8));
        assert!(filter.bits.iter().all(|&byte| byte == 0));
    }

    #[test]
    fn insert_sets_the_expected_bit_positions() {
        let mut filter = BloomFilter::new(100, 0.01);
        let key = b"hello";

        filter.insert(key);

        // recompute h_i by hand the same way insert does, and check each bit landed
        let h1 = BloomFilter::fnv_1a(key);
        let h2 = BloomFilter::djb2(key);

        for i in 0..filter.k {
            let combined = h1.wrapping_add((i as u64).wrapping_mul(h2));
            let pos = (combined % filter.m as u64) as usize;
            let byte_index = pos / 8;
            let bit_index = pos % 8;
            assert_ne!(
                filter.bits[byte_index] & (1 << bit_index),
                0,
                "expected bit {pos} to be set"
            );
        }
    }

    #[test]
    fn insert_is_deterministic() {
        let mut filter = BloomFilter::new(100, 0.01);
        filter.insert(b"repeat-me");
        let after_first_insert = filter.bits.clone();

        filter.insert(b"repeat-me");
        assert_eq!(
            filter.bits, after_first_insert,
            "inserting the same key twice must not change which bits are set"
        );
    }

    #[test]
    fn insert_different_keys_sets_more_bits() {
        let mut filter = BloomFilter::new(100, 0.01);
        let bits_before = filter.bits.clone();

        filter.insert(b"first-key");
        filter.insert(b"second-key");
        filter.insert(b"third-key");

        let total_bits_set: u32 = filter.bits.iter().map(|b| b.count_ones()).sum();
        let bits_before_set: u32 = bits_before.iter().map(|b| b.count_ones()).sum();
        assert!(total_bits_set > bits_before_set);
    }
}
