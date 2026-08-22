//! Monetary amounts.
//!
//! All money in Skattjakt is stored as an integer number of *öre* (1/100 SEK).
//! Binary floating point is never used for a value that reaches the user: an
//! economic estimate that is off by a rounding artefact undermines the whole
//! premise of an evidence-backed number.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

/// ISO 4217 code. Skattjakt is Sweden-only for the beta, but the currency is
/// carried explicitly so a future jurisdiction cannot silently inherit SEK.
pub const SEK: &str = "SEK";

/// An exact monetary amount, in öre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Money {
    ore: i64,
}

impl Money {
    pub const ZERO: Money = Money { ore: 0 };

    pub const fn from_ore(ore: i64) -> Self {
        Self { ore }
    }

    /// Whole kronor. Rejects amounts that would overflow i64 öre.
    pub fn from_sek(sek: i64) -> CoreResult<Self> {
        sek.checked_mul(100)
            .map(Self::from_ore)
            .ok_or(CoreError::MoneyOverflow)
    }

    pub const fn ore(self) -> i64 {
        self.ore
    }

    /// Kronor, truncated toward zero. For display only — never for arithmetic.
    pub const fn whole_sek(self) -> i64 {
        self.ore / 100
    }

    pub const fn is_zero(self) -> bool {
        self.ore == 0
    }

    pub const fn is_positive(self) -> bool {
        self.ore > 0
    }

    pub fn checked_add(self, other: Money) -> CoreResult<Money> {
        self.ore
            .checked_add(other.ore)
            .map(Self::from_ore)
            .ok_or(CoreError::MoneyOverflow)
    }

    pub fn checked_sub(self, other: Money) -> CoreResult<Money> {
        self.ore
            .checked_sub(other.ore)
            .map(Self::from_ore)
            .ok_or(CoreError::MoneyOverflow)
    }

    /// Multiply by a rate in basis points (1 bp = 0.01%), rounding half away
    /// from zero. Tax rates are expressed in basis points so that a rate like
    /// 20.6% is exact rather than a float approximation.
    pub fn mul_basis_points(self, bp: i64) -> CoreResult<Money> {
        let numerator = self.ore.checked_mul(bp).ok_or(CoreError::MoneyOverflow)?;
        Ok(Self::from_ore(div_round_half_away(numerator, 10_000)))
    }

    pub fn abs(self) -> CoreResult<Money> {
        self.ore
            .checked_abs()
            .map(Self::from_ore)
            .ok_or(CoreError::MoneyOverflow)
    }

    pub fn min(self, other: Money) -> Money {
        if self.ore <= other.ore {
            self
        } else {
            other
        }
    }

    pub fn max(self, other: Money) -> Money {
        if self.ore >= other.ore {
            self
        } else {
            other
        }
    }
}

/// Integer division rounding halves away from zero, so that 0.5 öre never
/// silently disappears in one direction only.
fn div_round_half_away(numerator: i64, denominator: i64) -> i64 {
    debug_assert!(denominator > 0);
    let half = denominator / 2;
    if numerator >= 0 {
        (numerator + half) / denominator
    } else {
        (numerator - half) / denominator
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.ore < 0;
        let abs = self.ore.unsigned_abs();
        let kronor = abs / 100;
        let ore = abs % 100;
        if negative {
            write!(f, "-")?;
        }
        // Swedish thousands grouping with a non-breaking space.
        let digits = kronor.to_string();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                write!(f, "\u{00a0}")?;
            }
            write!(f, "{ch}")?;
        }
        write!(f, ",{ore:02} kr")
    }
}

/// An inclusive estimate interval.
///
/// Skattjakt never states a single figure for an economic effect. Section 13 of
/// the product spec is explicit: an estimate is a range, or it is not shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoneyRange {
    pub low: Money,
    pub high: Money,
}

impl MoneyRange {
    /// Builds a range, normalising the bounds so `low <= high` always holds.
    pub fn new(a: Money, b: Money) -> Self {
        if a.ore() <= b.ore() {
            Self { low: a, high: b }
        } else {
            Self { low: b, high: a }
        }
    }

    pub fn exact(value: Money) -> Self {
        Self {
            low: value,
            high: value,
        }
    }

    /// Zero kronor. The identity for `checked_add`, and the honest answer when
    /// there is nothing to report — not an unknown.
    pub const ZERO: MoneyRange = MoneyRange {
        low: Money::ZERO,
        high: Money::ZERO,
    };

    /// Widens a point estimate by a symmetric uncertainty in basis points.
    /// Used when a calculation is deterministic but its *inputs* are partly
    /// assumed, which is the normal case for a preliminary year-end.
    pub fn around(value: Money, uncertainty_bp: i64) -> CoreResult<Self> {
        let delta = value.abs()?.mul_basis_points(uncertainty_bp)?;
        Ok(Self::new(
            value.checked_sub(delta)?,
            value.checked_add(delta)?,
        ))
    }

    pub fn checked_add(self, other: MoneyRange) -> CoreResult<MoneyRange> {
        Ok(Self {
            low: self.low.checked_add(other.low)?,
            high: self.high.checked_add(other.high)?,
        })
    }

    pub fn is_zero(self) -> bool {
        self.low.is_zero() && self.high.is_zero()
    }

    /// Midpoint, used only for ranking. Never displayed as "the" number.
    pub fn midpoint(self) -> Money {
        Money::from_ore(div_round_half_away(
            self.low.ore().saturating_add(self.high.ore()),
            2,
        ))
    }
}

impl Default for MoneyRange {
    /// Zero, so a deserialised report that predates a field reads as "nothing
    /// here" rather than failing to parse.
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Display for MoneyRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.low == self.high {
            write!(f, "{}", self.low)
        } else {
            write!(f, "{}–{}", self.low, self.high)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_sek_converts_to_ore() {
        assert_eq!(Money::from_sek(150).unwrap().ore(), 15_000);
    }

    #[test]
    fn from_sek_rejects_overflow() {
        assert!(Money::from_sek(i64::MAX).is_err());
    }

    #[test]
    fn basis_point_multiplication_is_exact_for_corporate_tax() {
        // 20.6 % of 100 000 kr = 20 600 kr, exactly.
        let profit = Money::from_sek(100_000).unwrap();
        assert_eq!(
            profit.mul_basis_points(2060).unwrap(),
            Money::from_sek(20_600).unwrap()
        );
    }

    #[test]
    fn rounding_is_half_away_from_zero_in_both_directions() {
        // 1 öre * 50 % = 0.5 öre -> 1 öre, and -1 öre * 50 % -> -1 öre.
        assert_eq!(Money::from_ore(1).mul_basis_points(5000).unwrap().ore(), 1);
        assert_eq!(
            Money::from_ore(-1).mul_basis_points(5000).unwrap().ore(),
            -1
        );
    }

    #[test]
    fn range_normalises_inverted_bounds() {
        let r = MoneyRange::new(Money::from_sek(900).unwrap(), Money::from_sek(100).unwrap());
        assert_eq!(r.low, Money::from_sek(100).unwrap());
        assert_eq!(r.high, Money::from_sek(900).unwrap());
    }

    #[test]
    fn range_around_widens_symmetrically() {
        let r = MoneyRange::around(Money::from_sek(1_000).unwrap(), 2_000).unwrap();
        assert_eq!(r.low, Money::from_sek(800).unwrap());
        assert_eq!(r.high, Money::from_sek(1_200).unwrap());
    }

    #[test]
    fn display_groups_thousands_and_keeps_ore() {
        assert_eq!(
            Money::from_ore(12_345_678).to_string(),
            "123\u{a0}456,78 kr"
        );
        assert_eq!(Money::from_ore(-5000).to_string(), "-50,00 kr");
        assert_eq!(Money::from_ore(0).to_string(), "0,00 kr");
    }

    #[test]
    fn ranges_sum_bound_wise() {
        let a = MoneyRange::new(Money::from_sek(10).unwrap(), Money::from_sek(20).unwrap());
        let b = MoneyRange::new(Money::from_sek(5).unwrap(), Money::from_sek(7).unwrap());
        let sum = a.checked_add(b).unwrap();
        assert_eq!(sum.low, Money::from_sek(15).unwrap());
        assert_eq!(sum.high, Money::from_sek(27).unwrap());
    }
}
