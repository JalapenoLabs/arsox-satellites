// Copyright © 2026 Jalapeno Labs

//! Adding up what several harness sessions in one turn spent.
//!
//! A turn is usually one harness run, and then it is not: a failing checker
//! wakes the agent back up, and that resumed session asks the same models the
//! same way through the same proxy grant. What it spends is spent on this turn.
//!
//! Reporting only the first session's accounting would understate every turn
//! that had to fix a checker, in the one number a cost reconciliation reads and
//! the one the thread's lifetime cost ceiling is enforced against. So the later
//! session's totals are folded in here rather than dropped or overwritten.
//!
//! # Absent is not zero
//!
//! Every optional count in the contract means "this harness did not report it",
//! which is a different fact from a reported zero. A fold that defaulted absent
//! to zero would turn "no cache accounting" into "read nothing from cache" the
//! moment a turn ran a second session. Two absent counts stay absent; anything
//! else adds, treating the missing side as nothing added.

use arsox_harness::HarnessResult;
use arsox_sdk::proto::common::v1::{Duration, Money};
use arsox_sdk::proto::turn::v1::TurnTiming;
use arsox_sdk::proto::usage::v1::{CostEstimate, ModelStatistics, ServerToolUsage, TokenUsage};

/// Billionths in one currency unit, which is how `Money` splits an amount.
const NANOS_PER_UNIT: i128 = 1_000_000_000;

/// Nanoseconds in one second, which is how `Duration` splits a span.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// What separates one session's account of itself from the next.
///
/// Both are the agent's own words about this turn, so neither is discarded: the
/// first says what was built and the second says how a failing check was
/// answered, and a reader wants both.
const SUMMARY_JOIN: &str = "\n\n---\n\n";

/// Folds a later session's accounting into the turn's running result.
///
/// The turn's verdict stays with the session that did the work: `is_error` and
/// `stop_reason` are not overwritten, because a fix cycle's outcome is reported
/// by the checker stage and its incidents rather than by silently rewriting what
/// the turn concluded. What accumulates is everything that was spent.
pub(super) fn fold(into: &mut Option<HarnessResult>, later: HarnessResult) {
    let Some(first) = into.as_mut() else {
        *into = Some(later);
        return;
    };

    if !later.summary.trim().is_empty() {
        if !first.summary.trim().is_empty() {
            first.summary.push_str(SUMMARY_JOIN);
        }
        first.summary.push_str(&later.summary);
    }

    fold_tokens(&mut first.tokens, &later.tokens);
    fold_cost(&mut first.cost, later.cost);
    fold_by_model(&mut first.by_model, later.by_model);
    fold_timing(&mut first.timing, &later.timing);

    first.permission_denials.extend(later.permission_denials);
}

/// Adds two token counts, keeping absence where neither side reported one.
fn fold_tokens(into: &mut TokenUsage, later: &TokenUsage) {
    into.input_tokens = into.input_tokens.saturating_add(later.input_tokens);
    into.output_tokens = into.output_tokens.saturating_add(later.output_tokens);
    into.total_tokens = into.total_tokens.saturating_add(later.total_tokens);

    into.cache_read_tokens = add_reported(into.cache_read_tokens, later.cache_read_tokens);
    into.cache_write_tokens = add_reported(into.cache_write_tokens, later.cache_write_tokens);
    into.reasoning_output_tokens =
        add_reported(into.reasoning_output_tokens, later.reasoning_output_tokens);
}

/// Adds two cost estimates, and says so when the sum is only a floor.
///
/// One session priced and the other not makes the total partial, because the
/// number is now smaller than what was actually spent. That is exactly what
/// `is_partial` exists to say, and losing it would present a floor as a total.
fn fold_cost(into: &mut CostEstimate, later: CostEstimate) {
    let priced_differently = into.amount.is_some() != later.amount.is_some();
    let mut incomparable = false;

    into.amount = match (into.amount.take(), later.amount) {
        (Some(first), Some(second)) if first.currency_code == second.currency_code => {
            Some(sum_money(&first, &second))
        }
        // Two currencies have no sum. Both sessions in a turn go through one
        // proxy so this cannot happen today, and inventing an exchange rate to
        // make it impossible would be worse than reporting a floor.
        (Some(first), Some(_incomparable)) => {
            incomparable = true;
            Some(first)
        }
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    };

    into.is_partial = into.is_partial || later.is_partial || priced_differently || incomparable;
}

/// Merges per-model totals, so a model that answered twice appears once.
///
/// Matched by model identifier rather than appended, because the point of
/// `by_model` is what each model cost. Two entries for one model would make a
/// failover look like it touched twice as many endpoints as it did.
fn fold_by_model(into: &mut Vec<ModelStatistics>, later: Vec<ModelStatistics>) {
    for statistics in later {
        let Some(held) = into.iter_mut().find(|held| held.model == statistics.model) else {
            into.push(statistics);
            continue;
        };

        held.tokens = match (held.tokens.take(), statistics.tokens) {
            (Some(mut first), Some(second)) => {
                fold_tokens(&mut first, &second);
                Some(first)
            }
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        };

        held.cost = match (held.cost.take(), statistics.cost) {
            (Some(mut first), Some(second)) => {
                fold_cost(&mut first, second);
                Some(first)
            }
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        };

        held.server_tools = match (held.server_tools.take(), statistics.server_tools) {
            (Some(first), Some(second)) => Some(ServerToolUsage {
                web_search_requests: add_small_reported(
                    first.web_search_requests,
                    second.web_search_requests,
                ),
                web_fetch_requests: add_small_reported(
                    first.web_fetch_requests,
                    second.web_fetch_requests,
                ),
            }),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        };
    }
}

/// Adds two timings, keeping the turn's perceived responsiveness intact.
///
/// `time_to_first_token` is the first session's and stays that way. It is the
/// gap a human waited before anything appeared, and a fix cycle's first token
/// arrives long after that question has been answered.
fn fold_timing(into: &mut TurnTiming, later: &TurnTiming) {
    into.total = add_spans(into.total.take(), later.total.as_ref());
    into.llm = add_spans(into.llm.take(), later.llm.as_ref());
    into.model_round_trips = add_small_reported(into.model_round_trips, later.model_round_trips);
}

/// Adds two counts that may be absent, keeping absence when neither reported.
fn add_reported(first: Option<u64>, later: Option<u64>) -> Option<u64> {
    match (first, later) {
        (None, None) => None,
        _reported => Some(first.unwrap_or(0).saturating_add(later.unwrap_or(0))),
    }
}

/// The same, for the 32-bit counts the contract uses for request tallies.
fn add_small_reported(first: Option<u32>, later: Option<u32>) -> Option<u32> {
    match (first, later) {
        (None, None) => None,
        _reported => Some(first.unwrap_or(0).saturating_add(later.unwrap_or(0))),
    }
}

/// Sums two amounts in one currency, carrying billionths into whole units.
///
/// Through `i128` rather than through a float. A single model request costs a
/// small fraction of a cent, and a ceiling that drifts from the invoice it was
/// meant to predict is exactly what integer money exists to prevent.
fn sum_money(first: &Money, second: &Money) -> Money {
    let total = first.to_nanos() + second.to_nanos();

    Money {
        currency_code: first.currency_code.clone(),
        // Saturating rather than wrapping: an amount this large is already a
        // defect, and wrapping would report a negative cost for it.
        units: i64::try_from(total / NANOS_PER_UNIT).unwrap_or(i64::MAX),
        // A remainder of a division by a billion always fits.
        nanos: i32::try_from(total % NANOS_PER_UNIT).unwrap_or_default(),
    }
}

/// Adds two spans, keeping absence when neither side reported one.
fn add_spans(first: Option<Duration>, later: Option<&Duration>) -> Option<Duration> {
    match (first, later) {
        (None, None) => None,
        (Some(only), None) => Some(only),
        (None, Some(only)) => Some(*only),
        (Some(first), Some(later)) => {
            let nanos = i64::from(first.nanos) + i64::from(later.nanos);

            Some(Duration {
                seconds: first
                    .seconds
                    .saturating_add(later.seconds)
                    .saturating_add(nanos / NANOS_PER_SECOND),
                // The carry above took the whole seconds, so what is left is
                // sub-second by construction.
                nanos: i32::try_from(nanos % NANOS_PER_SECOND).unwrap_or_default(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(input: u64, output: u64, cost_nanos: i32) -> HarnessResult {
        HarnessResult {
            summary: String::new(),
            tokens: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                total_tokens: input + output,
                ..TokenUsage::default()
            },
            cost: CostEstimate {
                amount: Some(Money::usd(0, cost_nanos)),
                is_partial: false,
            },
            ..HarnessResult::default()
        }
    }

    #[test]
    fn a_fix_cycles_tokens_are_added_to_the_turns() {
        // The whole point: a turn that had to fix a checker spent both sessions,
        // and a report showing one of them understates the bill.
        let mut turn = Some(session(100, 20, 0));

        fold(&mut turn, session(30, 5, 0));

        let tokens = turn.expect("folded").tokens;
        assert_eq!(tokens.input_tokens, 130);
        assert_eq!(tokens.output_tokens, 25);
        assert_eq!(tokens.total_tokens, 155);
    }

    #[test]
    fn a_count_neither_session_reported_stays_absent() {
        // Absent means the harness has no cache accounting. Folding it to zero
        // would say it read nothing from cache, which is a different claim and
        // a silent defect in a billing-adjacent number.
        let mut turn = Some(session(10, 1, 0));

        fold(&mut turn, session(10, 1, 0));

        assert_eq!(turn.expect("folded").tokens.cache_read_tokens, None);
    }

    #[test]
    fn a_count_one_session_reported_survives_the_fold() {
        let mut first = session(10, 1, 0);
        first.tokens.cache_read_tokens = Some(64);
        let mut turn = Some(first);

        fold(&mut turn, session(10, 1, 0));

        assert_eq!(turn.expect("folded").tokens.cache_read_tokens, Some(64));
    }

    #[test]
    fn costs_add_in_integers_and_carry_into_units() {
        let mut turn = Some(session(1, 1, 800_000_000));

        fold(&mut turn, session(1, 1, 300_000_000));

        let amount = turn
            .expect("folded")
            .cost
            .amount
            .expect("both sessions were priced");
        assert_eq!(amount.units, 1);
        assert_eq!(amount.nanos, 100_000_000);
    }

    #[test]
    fn a_total_missing_one_sessions_price_reports_itself_as_partial() {
        // It is a floor rather than a total, and presenting a floor as a total
        // is how a cost reconciliation quietly comes up short.
        let mut turn = Some(session(1, 1, 500_000_000));
        let mut unpriced = session(1, 1, 0);
        unpriced.cost.amount = None;

        fold(&mut turn, unpriced);

        assert!(turn.expect("folded").cost.is_partial);
    }

    #[test]
    fn a_model_that_answered_in_both_sessions_appears_once() {
        // Two entries for one model would make a failover look like it touched
        // twice as many endpoints as it did.
        let mut first = session(1, 1, 0);
        first.by_model = vec![ModelStatistics {
            model: "claude-opus-5".to_owned(),
            tokens: Some(TokenUsage {
                input_tokens: 10,
                ..TokenUsage::default()
            }),
            ..ModelStatistics::default()
        }];

        let mut later = session(1, 1, 0);
        later.by_model = vec![
            ModelStatistics {
                model: "claude-opus-5".to_owned(),
                tokens: Some(TokenUsage {
                    input_tokens: 5,
                    ..TokenUsage::default()
                }),
                ..ModelStatistics::default()
            },
            ModelStatistics {
                model: "claude-haiku-5".to_owned(),
                ..ModelStatistics::default()
            },
        ];

        let mut turn = Some(first);
        fold(&mut turn, later);

        let by_model = turn.expect("folded").by_model;
        assert_eq!(by_model.len(), 2);
        assert_eq!(
            by_model[0].tokens.as_ref().expect("tokens").input_tokens,
            15
        );
        assert_eq!(by_model[1].model, "claude-haiku-5");
    }

    #[test]
    fn both_sessions_accounts_of_the_turn_survive() {
        let mut first = session(1, 1, 0);
        first.summary = "built the endpoint".to_owned();
        let mut later = session(1, 1, 0);
        later.summary = "fixed the failing lint".to_owned();

        let mut turn = Some(first);
        fold(&mut turn, later);

        let summary = turn.expect("folded").summary;
        assert!(summary.contains("built the endpoint"));
        assert!(summary.contains("fixed the failing lint"));
    }

    #[test]
    fn the_turns_verdict_is_not_rewritten_by_a_fix_cycle() {
        // The checker stage reports what a fix cycle did. Letting it overwrite
        // `is_error` would mean a turn's status came from whichever session
        // happened to run last.
        let mut first = session(1, 1, 0);
        first.is_error = true;
        let mut turn = Some(first);

        fold(&mut turn, session(1, 1, 0));

        assert!(turn.expect("folded").is_error);
    }

    #[test]
    fn wall_clock_spans_add_with_their_carry() {
        let mut first = session(1, 1, 0);
        first.timing.total = Some(Duration {
            seconds: 2,
            nanos: 800_000_000,
        });
        let mut later = session(1, 1, 0);
        later.timing.total = Some(Duration {
            seconds: 1,
            nanos: 300_000_000,
        });

        let mut turn = Some(first);
        fold(&mut turn, later);

        let total = turn.expect("folded").timing.total.expect("both timed");
        assert_eq!(total.seconds, 4);
        assert_eq!(total.nanos, 100_000_000);
    }

    #[test]
    fn the_first_session_stands_alone_when_there_is_nothing_to_fold_into() {
        let mut turn = None;

        fold(&mut turn, session(7, 3, 0));

        assert_eq!(turn.expect("adopted").tokens.total_tokens, 10);
    }
}
