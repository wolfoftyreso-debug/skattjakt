//! Company identity and profile.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::ids::CompanyId;

/// A Swedish organisationsnummer, stored canonically as ten digits without a
/// separator.
///
/// Validated with the Luhn checksum that Skatteverket applies to the number, so
/// a typo is caught at the edge of the system rather than surfacing later as an
/// analysis attributed to the wrong company.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OrgNumber(String);

impl OrgNumber {
    pub fn parse(input: &str) -> CoreResult<Self> {
        let digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();

        // A 12-digit form is accepted only with the "16" century prefix that
        // Swedish legal persons use; anything else is a different kind of number.
        let ten = match digits.len() {
            10 => digits,
            12 if digits.starts_with("16") => digits[2..].to_string(),
            other => {
                return Err(CoreError::InvalidOrgNumber {
                    reason: format!("expected 10 digits (or 12 with a 16-prefix), got {other}"),
                })
            }
        };

        if !luhn_is_valid(&ten) {
            return Err(CoreError::InvalidOrgNumber {
                reason: "checksum digit does not match".to_string(),
            });
        }

        Ok(Self(ten))
    }

    pub fn as_digits(&self) -> &str {
        &self.0
    }

    /// The conventional `NNNNNN-NNNN` presentation.
    pub fn formatted(&self) -> String {
        format!("{}-{}", &self.0[..6], &self.0[6..])
    }

    /// Swedish limited companies (aktiebolag) carry group number 5. This is a
    /// strong hint, not a guarantee, so it informs onboarding rather than
    /// gating it.
    pub fn looks_like_aktiebolag(&self) -> bool {
        self.0.starts_with('5')
    }
}

impl From<OrgNumber> for String {
    fn from(value: OrgNumber) -> Self {
        value.0
    }
}

impl TryFrom<String> for OrgNumber {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl std::fmt::Display for OrgNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.formatted())
    }
}

fn luhn_is_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    for (i, ch) in digits.chars().enumerate() {
        let Some(d) = ch.to_digit(10) else { return false };
        // Leftmost digit is doubled for a ten-digit number.
        let v = if i % 2 == 0 { d * 2 } else { d };
        sum += if v > 9 { v - 9 } else { v };
    }
    sum % 10 == 0
}

/// A financial year. Swedish companies may use a broken fiscal year, and a
/// first or final year may be shortened or extended, so the period is stored as
/// explicit dates rather than a calendar year.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiscalYear {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl FiscalYear {
    pub fn new(start: NaiveDate, end: NaiveDate) -> CoreResult<Self> {
        if end <= start {
            return Err(CoreError::InvalidFiscalYear {
                reason: "the year must end after it starts".to_string(),
            });
        }
        let months = months_between(start, end);
        // Bokföringslagen allows an extended first or final year, capped at 18
        // months. A longer span means the input is wrong, not exotic.
        if months > 18 {
            return Err(CoreError::InvalidFiscalYear {
                reason: format!("a fiscal year cannot exceed 18 months, got about {months}"),
            });
        }
        Ok(Self { start, end })
    }

    /// A full calendar year, the most common case.
    pub fn calendar(year: i32) -> CoreResult<Self> {
        let start =
            NaiveDate::from_ymd_opt(year, 1, 1).ok_or_else(|| CoreError::InvalidFiscalYear {
                reason: format!("no such year: {year}"),
            })?;
        let end =
            NaiveDate::from_ymd_opt(year, 12, 31).ok_or_else(|| CoreError::InvalidFiscalYear {
                reason: format!("no such year: {year}"),
            })?;
        Self::new(start, end)
    }

    /// The taxation year (`taxeringsår`/`beskattningsår`) a rule set is keyed
    /// on: the calendar year in which the fiscal year ends.
    pub fn tax_year(&self) -> i32 {
        self.end.year()
    }

    pub fn is_calendar_year(&self) -> bool {
        self.start.month() == 1 && self.start.day() == 1 && self.end.month() == 12 && self.end.day() == 31
    }

    pub fn label(&self) -> String {
        if self.is_calendar_year() {
            self.start.year().to_string()
        } else {
            format!("{}–{}", self.start.format("%Y-%m-%d"), self.end.format("%Y-%m-%d"))
        }
    }
}

fn months_between(start: NaiveDate, end: NaiveDate) -> i32 {
    (end.year() - start.year()) * 12 + end.month() as i32 - start.month() as i32
}

/// What onboarding knows about the company.
///
/// Every field beyond name and organisationsnummer is optional on purpose:
/// section 3 of the product spec requires onboarding to ask a few questions and
/// only follow up when an answer is actually needed. `None` means "not asked or
/// not answered" and is treated as missing information, never as "no".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompanyProfile {
    pub id: CompanyId,
    pub name: String,
    pub org_number: OrgNumber,
    pub fiscal_year: FiscalYear,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub industry: Option<String>,
    /// SNI code, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sni_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub employee_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_count: Option<u32>,
    /// Whether the company belongs to a group (koncern).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_group: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operations_outside_sweden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub does_development_work: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owns_premises: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_vehicles: Option<bool>,
    /// Whether owners are active in the company to a significant extent, which
    /// is what makes the shares qualified for K10 purposes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owners_active_in_company: Option<bool>,
}

impl CompanyProfile {
    /// Profile questions that have not been answered yet. Drives both the
    /// dynamic onboarding follow-ups and the "missing information" section of
    /// the report.
    pub fn unanswered_fields(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.industry.is_none() {
            missing.push("industry");
        }
        if self.employee_count.is_none() {
            missing.push("employee_count");
        }
        if self.owner_count.is_none() {
            missing.push("owner_count");
        }
        if self.in_group.is_none() {
            missing.push("in_group");
        }
        if self.operations_outside_sweden.is_none() {
            missing.push("operations_outside_sweden");
        }
        if self.does_development_work.is_none() {
            missing.push("does_development_work");
        }
        if self.owns_premises.is_none() {
            missing.push("owns_premises");
        }
        if self.has_vehicles.is_none() {
            missing.push("has_vehicles");
        }
        if self.owners_active_in_company.is_none() {
            missing.push("owners_active_in_company");
        }
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_valid_org_number_in_several_formats() {
        // 556016-0680 (Volvo AB) is a real, published, Luhn-valid number.
        for input in ["5560160680", "556016-0680", "16556016-0680", "556016 0680"] {
            let n = OrgNumber::parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(n.as_digits(), "5560160680");
            assert_eq!(n.formatted(), "556016-0680");
        }
    }

    #[test]
    fn rejects_a_bad_checksum() {
        assert!(OrgNumber::parse("5560160681").is_err());
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(OrgNumber::parse("55601606").is_err());
        assert!(OrgNumber::parse("195560160680").is_err());
    }

    #[test]
    fn identifies_aktiebolag_series() {
        assert!(OrgNumber::parse("556016-0680").unwrap().looks_like_aktiebolag());
    }

    #[test]
    fn calendar_fiscal_year_has_expected_tax_year() {
        let fy = FiscalYear::calendar(2025).unwrap();
        assert_eq!(fy.tax_year(), 2025);
        assert!(fy.is_calendar_year());
        assert_eq!(fy.label(), "2025");
    }

    #[test]
    fn broken_fiscal_year_takes_tax_year_from_its_end() {
        let fy = FiscalYear::new(
            NaiveDate::from_ymd_opt(2024, 7, 1).unwrap(),
            NaiveDate::from_ymd_opt(2025, 6, 30).unwrap(),
        )
        .unwrap();
        assert_eq!(fy.tax_year(), 2025);
        assert!(!fy.is_calendar_year());
    }

    #[test]
    fn rejects_inverted_and_overlong_fiscal_years() {
        let a = NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
        let b = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        assert!(FiscalYear::new(a, b).is_err());

        let too_long = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        assert!(FiscalYear::new(a, too_long).is_err());
    }

    #[test]
    fn eighteen_month_extended_year_is_allowed() {
        let start = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2025, 6, 30).unwrap();
        assert!(FiscalYear::new(start, end).is_ok());
    }
}
