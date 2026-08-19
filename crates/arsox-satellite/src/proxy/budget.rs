// Copyright © 2026 Jalapeno Labs

//! Counting what a turn spends, and refusing once it has spent its ceiling.
//!
//! # Why the counting happens here
//!
//! A budget written into a prompt is a suggestion. This one is arithmetic on the
//! socket every completion travels over, so there is nothing for an agent to
//! route around: the [`Meter`] adds up what the provider itself reported, and
//! the proxy refuses the next request once the total reaches the ceiling.
//!
//! # Who counts and who decides
//!
//! The proxy counts. The runner decides how a turn ends. They are kept apart
//! deliberately: the proxy sees requests and knows nothing about whether the
//! turn is still alive, and an accountant that also wrote to the event log would
//! be emitting events for turns it cannot see the end of. So a [`Meter`] reports
//! [`Crossing`]s on a channel and the runner, which owns the turn's lifetime,
//! turns them into stream events and a graceful stop.
//!
//! # What counts as a token
//!
//! `input_tokens + output_tokens`, which is exactly the `total_tokens` the
//! canonical [`TokenUsage`] carries. Anthropic's `input_tokens` excludes what
//! came from cache, so a heavily cached turn spends fewer budgeted tokens than
//! it sent to the model.
//!
//! That is a deliberate choice of consistency over strictness. Counting cache
//! reads here would make the ceiling disagree with the total the same turn
//! reports in its result and in `GET /v1/statistics`, and an operator meeting
//! `BUDGET_TOKENS_EXHAUSTED` at four thousand tokens while the report says two
//! thousand has been handed a contradiction rather than a limit.
//!
//! [`TokenUsage`]: arsox_sdk::proto::usage::v1::TokenUsage

use arsox_sdk::proto::common::v1::{Money, cost_ceiling, duration_ceiling, token_ceiling};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::Ceiling;
use arsox_sdk::proto::settings::v1::Budget;
use serde_json::Value;
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

/// Percentage of a ceiling that earns a warning.
///
/// Documented in the README as 80%, and the number a host application builds its
/// reaction around. Changing it changes a published contract, not a heuristic.
pub const WARN_AT_PERCENT: u32 = 80;

/// The largest non-streamed response body the proxy buffers to read usage from.
///
/// A `POST /v1/messages` response that is not streamed is a single completion
/// and comfortably inside this. The bound exists so an endpoint returning
/// something unexpected cannot make the satellite hold it all in memory.
const MAX_SCANNED_JSON: usize = 4 * 1024 * 1024;

/// The longest server-sent-event line the scanner will hold before giving up on
/// it.
///
/// A `data:` line carrying a whole completion is large; one that never ends is a
/// stream the scanner should stop buffering rather than follow off a cliff.
const MAX_EVENT_LINE: usize = 4 * 1024 * 1024;

/// The ceilings one turn runs under.
///
/// Resolved from the thread's [`Budget`] once, at turn start. An unset ceiling
/// is no ceiling: the API is what requires a budget to be declared, and by the
/// time settings reach here the decision has already been made.
#[derive(Debug, Clone, Default)]
pub struct Ceilings {
    /// Tokens one turn may spend, across every agent in it.
    pub tokens_per_turn: Option<u64>,

    /// What the thread may spend across its entire life.
    pub cost_per_thread: Option<Money>,

    /// How long one turn may run.
    pub wall_clock_per_turn: Option<std::time::Duration>,
}

impl Ceilings {
    /// Reads the ceilings a thread declared.
    #[must_use]
    pub fn from_budget(budget: Option<&Budget>) -> Self {
        let Some(budget) = budget else {
            return Self::default();
        };

        Self {
            tokens_per_turn: budget.max_tokens_per_turn.as_ref().and_then(|ceiling| {
                match ceiling.ceiling.as_ref()? {
                    token_ceiling::Ceiling::Tokens(tokens) => Some(*tokens),
                    // `Unlimited` carries no data and exists so that an unbounded
                    // spend has to be typed out. Honoring it means no ceiling.
                    token_ceiling::Ceiling::Unlimited(_explicit) => None,
                }
            }),

            cost_per_thread: budget
                .max_cost_per_thread
                .as_ref()
                .and_then(|ceiling| match ceiling.ceiling.as_ref()? {
                    cost_ceiling::Ceiling::Cost(cost) => Some(cost.clone()),
                    cost_ceiling::Ceiling::Unlimited(_explicit) => None,
                }),

            wall_clock_per_turn: budget.max_wall_clock_per_turn.as_ref().and_then(|ceiling| {
                match ceiling.ceiling.as_ref()? {
                    duration_ceiling::Ceiling::Duration(span) => {
                        // A negative span is not a ceiling anybody meant, and
                        // treating it as zero would end every turn instantly.
                        let nanos = arsox_sdk::helpers::duration_to_nanos(span);
                        u64::try_from(nanos)
                            .ok()
                            .map(std::time::Duration::from_nanos)
                    }
                    duration_ceiling::Ceiling::Unlimited(_explicit) => None,
                }
            }),
        }
    }
}

/// A ceiling a turn has approached or reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crossing {
    /// The warning threshold, reported once per ceiling per turn.
    Approaching { ceiling: Ceiling, percent_used: u32 },

    /// Reached. Nothing further is allowed against this ceiling.
    Reached { ceiling: Ceiling },
}

/// The contract code a turn stopped by `ceiling` ends with.
///
/// One code per ceiling rather than a shared "budget exhausted", because a
/// caller that has to read a message string to learn which of three limits it
/// hit cannot act on any of them.
#[must_use]
pub const fn code_for(ceiling: Ceiling) -> ErrorCode {
    match ceiling {
        Ceiling::TokensPerTurn => ErrorCode::BudgetTokensExhausted,
        Ceiling::CostPerThread => ErrorCode::BudgetCostExhausted,
        // An unspecified ceiling cannot stop a turn, so the wall clock is the
        // only remaining arm rather than a guess.
        Ceiling::WallClockPerTurn | Ceiling::Unspecified => ErrorCode::BudgetWallClockExhausted,
    }
}

/// How much of a ceiling a spend has consumed, as a percentage.
///
/// Saturates at 100: a turn that overshot by one request has still consumed its
/// ceiling, and a percentage above 100 would only invite a client to render it.
#[must_use]
pub fn percent_used(spent: u128, ceiling: u128) -> u32 {
    if ceiling == 0 {
        // A ceiling of zero permits nothing, so anything at all has consumed it.
        return 100;
    }

    u32::try_from(spent.saturating_mul(100) / ceiling)
        .unwrap_or(100)
        .min(100)
}

/// How much of a cost ceiling a spend has consumed.
///
/// `None` when the two are not comparable. A ceiling denominated in a currency
/// nothing reports cannot be enforced, and inventing an exchange rate would be a
/// worse answer than declining to enforce and saying so.
#[must_use]
pub fn percent_of_cost(ceiling: &Money, spent_nanos: i128) -> Option<u32> {
    // Every harness reports cost in USD, so an empty code is read as USD rather
    // than refused: a caller building a ceiling by hand routinely leaves it off.
    if !ceiling.currency_code.is_empty() && ceiling.currency_code != "USD" {
        return None;
    }

    let ceiling = u128::try_from(ceiling.to_nanos()).unwrap_or(0);
    let spent = u128::try_from(spent_nanos).unwrap_or(0);

    Some(percent_used(spent, ceiling))
}

/// What one turn has spent, and what it is allowed to spend.
///
/// Shared between the proxy that counts and the runner that reacts. Every method
/// takes `&self` so both can hold the same meter behind an `Arc` without either
/// needing the other.
#[derive(Debug)]
pub struct Meter {
    /// The only ceiling a proxy can enforce per request.
    ///
    /// The other two are held nowhere near here on purpose. Wall clock and
    /// thread cost end a turn through the runner, and a meter carrying ceilings
    /// it does not enforce would read as though it did.
    tokens_per_turn: Option<u64>,

    state: Mutex<State>,
    crossings: UnboundedSender<Crossing>,
}

#[derive(Debug, Default)]
struct State {
    tokens: u64,
    warned: bool,
    reached: bool,
}

impl Meter {
    /// Builds a meter that reports its crossings on `crossings`.
    #[must_use]
    pub fn new(ceilings: &Ceilings, crossings: UnboundedSender<Crossing>) -> Self {
        Self {
            tokens_per_turn: ceilings.tokens_per_turn,
            state: Mutex::new(State::default()),
            crossings,
        }
    }

    /// A meter that counts but never refuses, for a turn that declared no
    /// ceilings.
    ///
    /// The receiving end is dropped, so crossings it can never produce go
    /// nowhere. Useful anywhere a grant is needed without a turn behind it.
    #[must_use]
    pub fn unmetered() -> Self {
        let (crossings, _nobody_listening) = tokio::sync::mpsc::unbounded_channel();
        Self::new(&Ceilings::default(), crossings)
    }

    /// Tokens this turn has spent so far.
    #[must_use]
    pub fn tokens_spent(&self) -> u64 {
        self.lock().tokens
    }

    /// The ceiling that has stopped this turn, if one has.
    ///
    /// Only ever the token ceiling, which is the one the proxy can enforce per
    /// request. Wall clock and thread cost end a turn through the runner, which
    /// tears the harness down rather than waiting for it to ask for another
    /// completion.
    ///
    /// Checked before a request is forwarded. Once this reports a ceiling it
    /// keeps reporting it, so a turn cannot spend its way back under the line
    /// with a response smaller than the one that crossed it.
    #[must_use]
    pub fn reached(&self) -> Option<Ceiling> {
        self.lock().reached.then_some(Ceiling::TokensPerTurn)
    }

    /// Adds what one response reported and reports any ceiling it crossed.
    ///
    /// Called after a response has finished streaming, because that is when the
    /// provider has finished saying what it cost. The request that crosses the
    /// ceiling is therefore always allowed to complete; it is the next one that
    /// is refused. Cutting a completion off mid-stream to save the overshoot
    /// would throw away tokens already paid for.
    pub fn record_tokens(&self, tokens: u64) {
        if tokens == 0 {
            return;
        }

        let crossing = {
            let mut state = self.lock();
            state.tokens = state.tokens.saturating_add(tokens);

            // Counted either way, so `tokens_spent` is true even for a turn that
            // declared no ceiling.
            match self.tokens_per_turn {
                None => None,
                Some(ceiling) => {
                    let used = percent_used(u128::from(state.tokens), u128::from(ceiling));

                    if state.tokens >= ceiling && !state.reached {
                        state.reached = true;
                        Some(Crossing::Reached {
                            ceiling: Ceiling::TokensPerTurn,
                        })
                    } else if used >= WARN_AT_PERCENT && !state.warned {
                        state.warned = true;
                        Some(Crossing::Approaching {
                            ceiling: Ceiling::TokensPerTurn,
                            percent_used: used,
                        })
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(crossing) = crossing {
            // A closed channel means the runner has already stopped caring,
            // which happens whenever a turn ends between the last request and
            // its accounting. The count still landed, which is the part that
            // matters.
            let _unheard = self.crossings.send(crossing);
        }
    }

    /// A poisoned lock means another thread panicked while holding it, and the
    /// counter it was updating is a `u64`. Recovering is strictly better than
    /// taking the satellite down over an integer.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Reads token usage out of a provider response as it streams past.
///
/// Anthropic reports usage in two shapes and the proxy meets both. A streamed
/// response carries it on `message_start` and again, cumulatively, on
/// `message_delta`; a non-streamed one carries it once at the top level of the
/// body. Which is arriving is decided by the response's own `Content-Type`
/// rather than guessed at from the bytes, because a JSON document and a
/// `data:` line are only distinguishable by luck.
#[derive(Debug)]
pub struct UsageReader {
    reading: Reading,
    input: u64,
    output: u64,
}

/// Where usage is read out of a response.
#[derive(Debug)]
enum Reading {
    /// Server-sent events: scan `data:` lines as they arrive.
    Events { pending: String },

    /// One JSON document, collected and parsed once it is whole.
    Json { collected: Vec<u8> },

    /// Something else, so nothing is read.
    Opaque,
}

impl UsageReader {
    /// Chooses how to read a response of this content type.
    #[must_use]
    pub fn for_content_type(content_type: Option<&str>) -> Self {
        let reading = match content_type {
            Some(value) if value.contains("text/event-stream") => Reading::Events {
                pending: String::new(),
            },
            Some(value) if value.contains("json") => Reading::Json {
                collected: Vec::new(),
            },
            _unreadable => Reading::Opaque,
        };

        Self {
            reading,
            input: 0,
            output: 0,
        }
    }

    /// Feeds one chunk of the response body.
    pub fn push(&mut self, chunk: &[u8]) {
        match &mut self.reading {
            Reading::Events { pending } => {
                // Lossy rather than strict: a multi-byte character split across
                // two chunks must not discard the line it sits in, and the
                // fields being read are all ASCII numbers.
                pending.push_str(&String::from_utf8_lossy(chunk));

                let mut usages = Vec::new();
                while let Some(end) = pending.find('\n') {
                    let line: String = pending.drain(..=end).collect();
                    if let Some(data) = line.trim_end().strip_prefix("data:")
                        && let Ok(event) = serde_json::from_str::<Value>(data.trim())
                    {
                        usages.push(event);
                    }
                }

                // A line that never ends is a stream to stop buffering rather
                // than follow off a cliff.
                if pending.len() > MAX_EVENT_LINE {
                    pending.clear();
                }

                for event in &usages {
                    self.observe(event);
                }
            }

            Reading::Json { collected } => {
                if collected.len() + chunk.len() <= MAX_SCANNED_JSON {
                    collected.extend_from_slice(chunk);
                } else {
                    // Past the bound nothing can be parsed from a partial
                    // document, so reading is abandoned rather than left to
                    // produce a confident zero from half a body.
                    self.reading = Reading::Opaque;
                }
            }

            Reading::Opaque => {}
        }
    }

    /// The tokens this response reported, once nothing more is coming.
    ///
    /// Consuming: a second call reports zero, so a body that both ends and is
    /// then dropped is counted exactly once.
    pub fn take_total(&mut self) -> u64 {
        if let Reading::Json { collected } = std::mem::replace(&mut self.reading, Reading::Opaque)
            && let Ok(body) = serde_json::from_slice::<Value>(&collected)
        {
            self.observe(&body);
        }

        self.reading = Reading::Opaque;

        let total = self.input.saturating_add(self.output);
        self.input = 0;
        self.output = 0;
        total
    }

    /// Folds one native event or response body into the running totals.
    ///
    /// Both counters take the largest value seen rather than a sum. Streaming
    /// reports output cumulatively on every `message_delta`, so adding them up
    /// would count the same tokens once per delta, and `message_start` repeats
    /// the input count that `message_delta` may carry again.
    fn observe(&mut self, event: &Value) {
        let usage = event
            .get("usage")
            .or_else(|| event.pointer("/message/usage"));

        let Some(usage) = usage else {
            return;
        };

        if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input = self.input.max(input);
        }
        if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output = self.output.max(output);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::common::v1::{CostCeiling, TokenCeiling, Unlimited};

    fn budget_with(tokens: u64) -> Budget {
        Budget {
            max_tokens_per_turn: Some(TokenCeiling {
                ceiling: Some(token_ceiling::Ceiling::Tokens(tokens)),
            }),
            ..Budget::default()
        }
    }

    #[test]
    fn an_unlimited_ceiling_resolves_to_no_ceiling_rather_than_zero() {
        // `Unlimited` exists so an unbounded spend has to be typed out. Reading
        // it as a ceiling of zero would turn the most permissive setting in the
        // contract into the most restrictive one.
        let ceilings = Ceilings::from_budget(Some(&Budget {
            max_tokens_per_turn: Some(TokenCeiling {
                ceiling: Some(token_ceiling::Ceiling::Unlimited(Unlimited {})),
            }),
            max_cost_per_thread: Some(CostCeiling {
                ceiling: Some(cost_ceiling::Ceiling::Unlimited(Unlimited {})),
            }),
            max_wall_clock_per_turn: None,
        }));

        assert_eq!(ceilings.tokens_per_turn, None);
        assert_eq!(ceilings.cost_per_thread, None);
        assert_eq!(ceilings.wall_clock_per_turn, None);
    }

    #[test]
    fn a_declared_token_ceiling_is_carried_through() {
        let ceilings = Ceilings::from_budget(Some(&budget_with(1_000)));

        assert_eq!(ceilings.tokens_per_turn, Some(1_000));
    }

    #[test]
    fn spending_past_eighty_percent_warns_once_and_only_once() {
        let (sender, mut crossings) = tokio::sync::mpsc::unbounded_channel();
        let meter = Meter::new(&Ceilings::from_budget(Some(&budget_with(1_000))), sender);

        meter.record_tokens(700);
        assert!(
            crossings.try_recv().is_err(),
            "seventy percent is not yet worth interrupting anybody over"
        );

        meter.record_tokens(100);
        assert_eq!(
            crossings.try_recv().expect("should warn at eighty percent"),
            Crossing::Approaching {
                ceiling: Ceiling::TokensPerTurn,
                percent_used: 80,
            }
        );

        meter.record_tokens(50);
        assert!(
            crossings.try_recv().is_err(),
            "a warning per request past the threshold would be noise, not a signal"
        );
    }

    #[test]
    fn reaching_the_ceiling_refuses_everything_after_it() {
        let (sender, mut crossings) = tokio::sync::mpsc::unbounded_channel();
        let meter = Meter::new(&Ceilings::from_budget(Some(&budget_with(1_000))), sender);

        meter.record_tokens(999);
        assert_eq!(meter.reached(), None);
        // The warning came first, at eighty percent.
        assert!(matches!(
            crossings.try_recv(),
            Ok(Crossing::Approaching { .. })
        ));

        meter.record_tokens(1);
        assert_eq!(meter.reached(), Some(Ceiling::TokensPerTurn));
        assert_eq!(
            crossings.try_recv().expect("should report the ceiling"),
            Crossing::Reached {
                ceiling: Ceiling::TokensPerTurn,
            }
        );

        // Once reached it stays reached: a smaller response afterwards must not
        // let the turn spend its way back under the line.
        meter.record_tokens(1);
        assert_eq!(meter.reached(), Some(Ceiling::TokensPerTurn));
    }

    #[test]
    fn a_turn_with_no_ceiling_still_counts_what_it_spent() {
        let meter = Meter::unmetered();

        meter.record_tokens(5_000);

        assert_eq!(meter.tokens_spent(), 5_000);
        assert_eq!(meter.reached(), None);
    }

    #[test]
    fn streamed_usage_is_read_from_the_events_that_carry_it() {
        // The shape Anthropic actually streams: input on `message_start`, output
        // reported cumulatively on each `message_delta`.
        let mut reader = UsageReader::for_content_type(Some("text/event-stream"));

        reader.push(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":120,\"cache_read_input_tokens\":900,\"output_tokens\":1}}}\n\n");
        reader.push(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":40}}\n\n");
        reader.push(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":85}}\n\n");

        // 120 input plus 85 output. The cumulative deltas are not summed, and
        // the 900 cache reads are not counted: `input_tokens` already excludes
        // them and the canonical total does too.
        assert_eq!(reader.take_total(), 205);
    }

    #[test]
    fn a_data_line_split_across_chunks_is_still_read() {
        // Chunk boundaries are the network's to choose, and a reader that only
        // works when a whole event lands at once undercounts silently.
        let mut reader = UsageReader::for_content_type(Some("text/event-stream"));

        reader.push(b"data: {\"type\":\"message_delta\",\"usa");
        reader.push(b"ge\":{\"input_tokens\":10,\"output_tokens\":7}}\n\n");

        assert_eq!(reader.take_total(), 17);
    }

    #[test]
    fn a_non_streamed_response_is_read_from_its_body() {
        let mut reader = UsageReader::for_content_type(Some("application/json"));

        reader.push(br#"{"type":"message","usage":{"input_tokens":30,"#);
        reader.push(br#""output_tokens":12}}"#);

        assert_eq!(reader.take_total(), 42);
    }

    #[test]
    fn a_response_carrying_no_usage_counts_nothing() {
        let mut reader = UsageReader::for_content_type(Some("application/json"));
        reader.push(br#"{"type":"error","error":{"type":"api_error"}}"#);

        assert_eq!(reader.take_total(), 0);
    }

    #[test]
    fn usage_is_committed_exactly_once() {
        // The body commits on drop as well as at the end of the stream, so a
        // second read has to report nothing or every aborted response would be
        // counted twice.
        let mut reader = UsageReader::for_content_type(Some("application/json"));
        reader.push(br#"{"usage":{"input_tokens":30,"output_tokens":12}}"#);

        assert_eq!(reader.take_total(), 42);
        assert_eq!(reader.take_total(), 0);
    }

    #[test]
    fn a_cost_ceiling_in_a_currency_nothing_reports_is_not_enforced() {
        // Inventing an exchange rate would be a worse answer than declining.
        let euros = Money {
            currency_code: "EUR".to_owned(),
            units: 10,
            nanos: 0,
        };

        assert_eq!(percent_of_cost(&euros, 5_000_000_000), None);
        assert_eq!(percent_of_cost(&Money::usd(10, 0), 5_000_000_000), Some(50));
    }

    #[test]
    fn percentages_saturate_rather_than_exceeding_the_ceiling() {
        assert_eq!(percent_used(0, 100), 0);
        assert_eq!(percent_used(80, 100), 80);
        assert_eq!(percent_used(400, 100), 100);
        // A ceiling of zero permits nothing, so anything has consumed all of it.
        assert_eq!(percent_used(1, 0), 100);
    }

    #[test]
    fn each_ceiling_ends_a_turn_with_its_own_code() {
        // A caller that has to read a message string to learn which of three
        // limits it hit cannot act on any of them.
        assert_eq!(
            code_for(Ceiling::TokensPerTurn),
            ErrorCode::BudgetTokensExhausted
        );
        assert_eq!(
            code_for(Ceiling::CostPerThread),
            ErrorCode::BudgetCostExhausted
        );
        assert_eq!(
            code_for(Ceiling::WallClockPerTurn),
            ErrorCode::BudgetWallClockExhausted
        );
    }
}
