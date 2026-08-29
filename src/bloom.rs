// To make a bloom filter we have to have two values, p and n: p being the rate of false
// positives we desire, and n being the amount of keys we expect.
// The formulas are:
//   m = -n * ln(p) / (ln 2)^2   (bit array size)
//   k = (m / n) * ln 2          (number of hash functions)
// So for p = 1% and n = 10000 we get m = 95,851 and k = 7, rounding both to the nearest
// integer since bit array size and hash function count must be whole numbers.
