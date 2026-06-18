//! Orbital single-event-upset (SEU) rates, for framing the reliability proof in
//! a space operator's own units.
//!
//! Radiation flips bits. The rate depends heavily on the orbit, the shielding,
//! and the memory device, so the numbers here are representative order-of-
//! magnitude figures for unshielded commodity memory, not a specification for a
//! particular part. The contribution of this module is not the exact rate (a
//! real deployment measures that for its hardware); it is the model that turns
//! the fault stress into a sentence a mission-assurance engineer can read:
//! "at the expected upset rate for this orbit, the store sees about N upsets a
//! day, and every one of them is detected."

/// A named orbit profile with a representative SEU rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orbit {
    /// Low Earth orbit (for example a 550 km sun-synchronous orbit).
    Leo,
    /// Geostationary orbit, outside most of the magnetosphere's protection.
    Geo,
}

impl Orbit {
    /// A representative SEU rate in upsets per bit per day for unshielded
    /// commodity memory. Order-of-magnitude only.
    #[must_use]
    pub const fn upsets_per_bit_day(self) -> f64 {
        match self {
            Self::Leo => 5.0e-7,
            Self::Geo => 1.0e-6,
        }
    }

    /// A human-readable name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Leo => "low Earth orbit",
            Self::Geo => "geostationary orbit",
        }
    }
}

/// Expected upsets per day for an artifact of `bytes` bytes at `orbit`.
#[must_use]
pub fn expected_upsets_per_day(bytes: usize, orbit: Orbit) -> f64 {
    // Reasonable artifact sizes are far inside f64's exact-integer range, so the
    // conversion does not lose anything that matters here.
    #[allow(clippy::cast_precision_loss)]
    let bits = bytes as f64 * 8.0;
    bits * orbit.upsets_per_bit_day()
}

#[cfg(test)]
mod tests {
    use super::{expected_upsets_per_day, Orbit};

    #[test]
    fn dose_scales_with_size_and_orbit() {
        // A bigger artifact and a harsher orbit both raise the expected dose.
        let tiny = expected_upsets_per_day(10_000, Orbit::Leo);
        let leo_large = expected_upsets_per_day(100_000, Orbit::Leo);
        let geo_large = expected_upsets_per_day(100_000, Orbit::Geo);
        assert!(leo_large > tiny);
        assert!(geo_large > leo_large);
        assert!(tiny > 0.0);
    }
}
