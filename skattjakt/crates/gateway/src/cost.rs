//! Cost accounting and budgets (sections 46, 68, 69).
//!
//! Every unit here is an integer in micro-öre. Öre is too coarse: a single
//! cheap call costs a fraction of one, would round to zero, and a thousand of
//! them would then cost nothing at all — the exact shape of a bill nobody
//! noticed accruing. The same objection to floating point that governs `Money`
//! governs this.
//!
//! Prices are configuration, not constants. A price compiled into the binary is
//! a price that is wrong the day after the provider changes it, and wrong
//! silently, which is worse than absent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use skattjakt_model::TokenUsage;

/// One thousandth of an öre, times a thousand. 1 SEK = 100 öre = 100 000 000
/// micro-öre.
pub const MICRO_ORE_PER_SEK: i64 = 100_000_000;

/// What a million tokens costs, by direction, for one model.
///
/// Read from configuration at startup. A model with no entry cannot be called
/// through the gateway at all: an unpriced call is an unbounded call, and the
/// budget check would silently pass for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Micro-öre per million input tokens.
    pub input_per_mtok: i64,
    /// Micro-öre per million output tokens.
    pub output_per_mtok: i64,
    /// Cached reads are usually cheaper; priced separately rather than assumed.
    #[serde(default)]
    pub cache_read_per_mtok: i64,
    #[serde(default)]
    pub cache_write_per_mtok: i64,
}

impl ModelPrice {
    /// Cost of one call, rounded up.
    ///
    /// Up, not to nearest: a budget that under-reports is a budget that gets
    /// exceeded. Every rounding decision in this file is made against the
    /// operator, and the operator is us.
    pub fn cost_micro_ore(&self, usage: &TokenUsage) -> i64 {
        const PER_MTOK: i64 = 1_000_000;
        let component = |tokens: u32, price: i64| -> i64 {
            let numerator = (tokens as i64).saturating_mul(price);
            // Ceiling division on non-negative values.
            numerator.saturating_add(PER_MTOK - 1) / PER_MTOK
        };
        component(usage.input_tokens, self.input_per_mtok)
            .saturating_add(component(usage.output_tokens, self.output_per_mtok))
            .saturating_add(component(
                usage.cache_read_input_tokens,
                self.cache_read_per_mtok,
            ))
            .saturating_add(component(
                usage.cache_creation_input_tokens,
                self.cache_write_per_mtok,
            ))
    }
}

/// The price list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PriceList {
    prices: BTreeMap<String, ModelPrice>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PriceError {
    #[error("no price is configured for model {0}; refusing to call an unpriced model")]
    Unpriced(String),
    #[error("price list could not be parsed: {0}")]
    Malformed(String),
    #[error("price for model {0} is negative")]
    Negative(String),
}

impl PriceList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, model: impl Into<String>, price: ModelPrice) -> Self {
        self.prices.insert(model.into(), price);
        self
    }

    /// Parses `SKATTJAKT_MODEL_PRICES`, a JSON object of model id to price.
    pub fn from_json(raw: &str) -> Result<Self, PriceError> {
        let prices: BTreeMap<String, ModelPrice> =
            serde_json::from_str(raw).map_err(|e| PriceError::Malformed(e.to_string()))?;
        for (model, price) in &prices {
            if price.input_per_mtok < 0
                || price.output_per_mtok < 0
                || price.cache_read_per_mtok < 0
                || price.cache_write_per_mtok < 0
            {
                return Err(PriceError::Negative(model.clone()));
            }
        }
        Ok(Self { prices })
    }

    pub fn get(&self, model: &str) -> Result<&ModelPrice, PriceError> {
        self.prices
            .get(model)
            .ok_or_else(|| PriceError::Unpriced(model.to_string()))
    }

    pub fn cost(&self, model: &str, usage: &TokenUsage) -> Result<i64, PriceError> {
        Ok(self.get(model)?.cost_micro_ore(usage))
    }

    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    pub fn models(&self) -> impl Iterator<Item = &String> {
        self.prices.keys()
    }
}

/// The spend ceiling for one analysis (section 69).
///
/// A ceiling per analysis rather than per month: a monthly cap is discovered on
/// the 28th, when it has already been paid. A per-analysis cap stops the one
/// run that went wrong — a document that makes the model loop, a retry storm —
/// while the rest of the service keeps working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub limit_micro_ore: i64,
    spent_micro_ore: i64,
    calls: u32,
    max_calls: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error("this analysis has reached its cost ceiling")]
    Exhausted,
    #[error("this analysis has reached its call ceiling")]
    TooManyCalls,
}

impl Budget {
    /// The default ceiling. Stated in SEK at the call site, because a number in
    /// micro-öre is unreadable and an unreadable limit is an unreviewed limit.
    pub const DEFAULT_LIMIT_SEK: i64 = 25;
    /// Two passes, a handful of tasks each, plus retries. A run needing more
    /// than this has gone wrong in a way more spending will not fix.
    pub const DEFAULT_MAX_CALLS: u32 = 24;

    pub fn new(limit_micro_ore: i64, max_calls: u32) -> Self {
        Self {
            limit_micro_ore: limit_micro_ore.max(1),
            spent_micro_ore: 0,
            calls: 0,
            max_calls: max_calls.max(1),
        }
    }

    pub fn from_sek(limit_sek: i64) -> Self {
        Self::new(
            limit_sek.saturating_mul(MICRO_ORE_PER_SEK),
            Self::DEFAULT_MAX_CALLS,
        )
    }

    /// Restores a budget from its stored counters, so a retried analysis
    /// resumes against what it has already spent rather than starting fresh.
    /// Without this, three attempts cost three budgets.
    pub fn resumed(limit_micro_ore: i64, spent_micro_ore: i64, calls: u32, max_calls: u32) -> Self {
        Self {
            limit_micro_ore: limit_micro_ore.max(1),
            spent_micro_ore: spent_micro_ore.max(0),
            calls,
            max_calls: max_calls.max(1),
        }
    }

    pub fn spent(&self) -> i64 {
        self.spent_micro_ore
    }

    pub fn calls(&self) -> u32 {
        self.calls
    }

    pub fn remaining(&self) -> i64 {
        (self.limit_micro_ore - self.spent_micro_ore).max(0)
    }

    /// Checked before a call, with the worst case that call could cost.
    ///
    /// Before, not after. Checking afterwards means the money is already spent,
    /// and an analysis that blows its budget on its first call would have no
    /// ceiling at all.
    pub fn check(&self, worst_case: i64) -> Result<(), BudgetError> {
        if self.calls >= self.max_calls {
            return Err(BudgetError::TooManyCalls);
        }
        if self.spent_micro_ore.saturating_add(worst_case) > self.limit_micro_ore {
            return Err(BudgetError::Exhausted);
        }
        Ok(())
    }

    /// Records what a call actually cost.
    pub fn charge(&mut self, cost_micro_ore: i64) {
        self.spent_micro_ore = self.spent_micro_ore.saturating_add(cost_micro_ore.max(0));
        self.calls += 1;
    }

    /// A charge for a call that failed. Still counted: a failed call bills the
    /// input tokens, and a retry loop that did not count its failures would
    /// spend without limit.
    pub fn charge_failed(&mut self, cost_micro_ore: i64) {
        self.charge(cost_micro_ore);
    }

    pub fn is_exhausted(&self) -> bool {
        self.spent_micro_ore >= self.limit_micro_ore || self.calls >= self.max_calls
    }

    /// For display. Öre, rounded up, so the figure shown is never less than
    /// what was spent.
    pub fn spent_ore(&self) -> i64 {
        (self.spent_micro_ore + 999_999) / 1_000_000
    }
}

/// The worst a single call could cost, used for the pre-flight check.
///
/// Input tokens are known before the call. Output tokens are not, so `max_tokens`
/// is assumed in full — the pessimistic assumption, which is the only safe one
/// for a ceiling.
pub fn worst_case_cost(price: &ModelPrice, input_tokens: u32, max_output_tokens: u32) -> i64 {
    price.cost_micro_ore(&TokenUsage {
        input_tokens,
        output_tokens: max_output_tokens,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    })
}

/// A rough token count for a pre-flight estimate.
///
/// Deliberately an overestimate. Real tokenisation is the provider's and is not
/// available locally; four characters per token underestimates for Swedish
/// text, so the divisor is three. The number is used only to decide whether to
/// *attempt* a call, and erring high stops a run early rather than letting one
/// through that then blows the ceiling.
pub fn estimate_tokens(text: &str) -> u32 {
    (text.len() / 3 + 1).min(u32::MAX as usize) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> ModelPrice {
        // 30 SEK per million input tokens, 150 per million output.
        ModelPrice {
            input_per_mtok: 30 * MICRO_ORE_PER_SEK,
            output_per_mtok: 150 * MICRO_ORE_PER_SEK,
            cache_read_per_mtok: 3 * MICRO_ORE_PER_SEK,
            cache_write_per_mtok: 37 * MICRO_ORE_PER_SEK,
        }
    }

    #[test]
    fn a_million_input_tokens_costs_the_stated_price() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        assert_eq!(price().cost_micro_ore(&usage), 30 * MICRO_ORE_PER_SEK);
    }

    #[test]
    fn small_calls_do_not_round_to_zero() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        assert!(price().cost_micro_ore(&usage) > 0);
    }

    #[test]
    fn a_thousand_tiny_calls_accumulate_rather_than_vanishing() {
        let usage = TokenUsage {
            input_tokens: 1,
            ..Default::default()
        };
        let single = price().cost_micro_ore(&usage);
        let mut budget = Budget::from_sek(1);
        for _ in 0..1_000 {
            budget.charge(single);
        }
        assert!(budget.spent() >= 1_000, "tiny calls rounded away");
    }

    #[test]
    fn cost_is_rounded_up_never_down() {
        let cheap = ModelPrice {
            input_per_mtok: 1,
            output_per_mtok: 0,
            cache_read_per_mtok: 0,
            cache_write_per_mtok: 0,
        };
        let usage = TokenUsage {
            input_tokens: 1,
            ..Default::default()
        };
        // 1 token at 1 micro-ore per million is 0.000001; charged as 1.
        assert_eq!(cheap.cost_micro_ore(&usage), 1);
    }

    #[test]
    fn cached_reads_are_priced_separately() {
        let with_cache = TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 1_000_000,
            cache_creation_input_tokens: 0,
        };
        assert_eq!(price().cost_micro_ore(&with_cache), 3 * MICRO_ORE_PER_SEK);
    }

    #[test]
    fn an_unpriced_model_cannot_be_costed() {
        let list = PriceList::new().with("known-model", price());
        assert!(matches!(
            list.cost("unknown-model", &TokenUsage::default()),
            Err(PriceError::Unpriced(_))
        ));
    }

    #[test]
    fn a_negative_price_is_rejected_at_load() {
        let raw = r#"{"m": {"input_per_mtok": -1, "output_per_mtok": 1}}"#;
        assert!(matches!(
            PriceList::from_json(raw),
            Err(PriceError::Negative(_))
        ));
    }

    #[test]
    fn a_price_list_round_trips_through_configuration() {
        let raw = r#"{"model-a": {"input_per_mtok": 3000000000, "output_per_mtok": 15000000000}}"#;
        let list = PriceList::from_json(raw).unwrap();
        assert!(list.get("model-a").is_ok());
    }

    #[test]
    fn the_budget_is_checked_before_the_call_not_after() {
        let budget = Budget::from_sek(1);
        let huge = 2 * MICRO_ORE_PER_SEK;
        assert_eq!(budget.check(huge), Err(BudgetError::Exhausted));
        // Nothing was spent: the call never happened.
        assert_eq!(budget.spent(), 0);
    }

    #[test]
    fn a_call_within_the_ceiling_is_allowed() {
        let budget = Budget::from_sek(25);
        assert!(budget.check(MICRO_ORE_PER_SEK).is_ok());
    }

    #[test]
    fn spending_accumulates_until_the_ceiling_is_reached() {
        let mut budget = Budget::from_sek(3);
        budget.charge(MICRO_ORE_PER_SEK);
        budget.charge(MICRO_ORE_PER_SEK);
        assert!(budget.check(MICRO_ORE_PER_SEK).is_ok());
        budget.charge(MICRO_ORE_PER_SEK);
        assert!(budget.is_exhausted());
        assert_eq!(budget.check(1), Err(BudgetError::Exhausted));
    }

    #[test]
    fn a_failed_call_still_costs_its_input_tokens() {
        let mut budget = Budget::from_sek(25);
        budget.charge_failed(MICRO_ORE_PER_SEK);
        assert_eq!(budget.spent(), MICRO_ORE_PER_SEK);
        assert_eq!(budget.calls(), 1);
    }

    #[test]
    fn a_retry_storm_hits_the_call_ceiling_even_when_each_call_is_free() {
        let mut budget = Budget::new(i64::MAX / 2, 5);
        for _ in 0..5 {
            assert!(budget.check(0).is_ok());
            budget.charge(0);
        }
        assert_eq!(budget.check(0), Err(BudgetError::TooManyCalls));
    }

    #[test]
    fn a_resumed_analysis_continues_against_what_it_already_spent() {
        let budget = Budget::resumed(3 * MICRO_ORE_PER_SEK, 2 * MICRO_ORE_PER_SEK, 4, 24);
        assert_eq!(budget.remaining(), MICRO_ORE_PER_SEK);
        assert_eq!(
            budget.check(2 * MICRO_ORE_PER_SEK),
            Err(BudgetError::Exhausted)
        );
    }

    #[test]
    fn the_worst_case_assumes_the_full_output_allowance() {
        let optimistic = worst_case_cost(&price(), 1_000, 0);
        let pessimistic = worst_case_cost(&price(), 1_000, 8_000);
        assert!(pessimistic > optimistic);
    }

    #[test]
    fn the_token_estimate_errs_high() {
        // 300 characters of Swedish is nearer 90 real tokens than 100; the
        // estimate is allowed to exceed it, never to fall short of a third.
        let text = "å".repeat(100); // 200 bytes
        assert!(estimate_tokens(&text) >= 66);
    }

    #[test]
    fn spent_ore_rounds_up_so_the_figure_shown_is_never_short() {
        let mut budget = Budget::from_sek(25);
        budget.charge(1);
        assert_eq!(budget.spent_ore(), 1);
    }

    #[test]
    fn arithmetic_saturates_rather_than_overflowing() {
        let absurd = ModelPrice {
            input_per_mtok: i64::MAX,
            output_per_mtok: i64::MAX,
            cache_read_per_mtok: 0,
            cache_write_per_mtok: 0,
        };
        let usage = TokenUsage {
            input_tokens: u32::MAX,
            output_tokens: u32::MAX,
            ..Default::default()
        };
        // The point is that it returns rather than panicking in release *or*
        // debug; a panic here would take down a worker on a malformed price.
        assert!(absurd.cost_micro_ore(&usage) > 0);
    }
}
