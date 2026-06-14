//! CRC32 (IEEE 802.3 polynomial, reflected), hand-written.
//!
//! Used by the page header to detect accidental on-disk corruption. NOT a
//! cryptographic hash - a determined attacker can craft collisions. This is
//! the same algorithm gzip and Ethernet use, suitable for catching bit-rot
//! and torn writes.
//!
//! # Why hand-written instead of `crc32fast`
//!
//! A core project rule is that anything storage-related is written from
//! scratch. CRC32 is small enough that a from-scratch impl is ~25 lines and
//! finishes in const context, so no dependency is justified. A
//! SIMD-accelerated version would be measurably faster on long
//! buffers, but the page checksum runs over 8 KiB at a time - not the
//! hot path that would benefit from intrinsics.

/// IEEE 802.3 / Ethernet polynomial in reflected form.
const POLY: u32 = 0xedb8_8320;

/// Lookup table for one-byte-at-a-time computation, generated at compile time.
const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut c = i;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i as usize] = c;
        i += 1;
    }
    table
};

/// Compute CRC32 of `bytes` using the IEEE polynomial, reflected, with the
/// standard initial value of `0xFFFF_FFFF` and final XOR of `0xFFFF_FFFF`.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        // The low byte of `crc` is exactly the table index we want - the
        // truncation cast is the whole point of the algorithm.
        #[allow(clippy::cast_possible_truncation)]
        let idx = ((crc as u8) ^ b) as usize;
        crc = (crc >> 8) ^ TABLE[idx];
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vector from the IEEE 802.3 spec: CRC32("123456789") = 0xCBF43926.
    #[test]
    fn matches_ieee_test_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn empty_input_is_zero() {
        // CRC32 of an empty input under (initial=!0, final XOR=!0) is 0.
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn single_bit_flip_changes_checksum() {
        let a = b"hello world".to_vec();
        let mut b = a.clone();
        b[0] ^= 0x01;
        assert_ne!(crc32(&a), crc32(&b));
    }

    #[test]
    fn determinism() {
        let buf = vec![0xa5u8; 4096];
        assert_eq!(crc32(&buf), crc32(&buf));
    }

    #[test]
    fn table_first_entries_known_values() {
        // First few entries of the standard IEEE reflected table.
        assert_eq!(TABLE[0], 0x0000_0000);
        assert_eq!(TABLE[1], 0x7707_3096);
        assert_eq!(TABLE[2], 0xee0e_612c);
    }
}
