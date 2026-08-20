// Copyright © 2026 Jalapeno Labs

//! Trying a turn's model endpoints in the order they were declared.
//!
//! # Strict order, never a pool
//!
//! A thread's endpoints are a failover list rather than a load balancer. The
//! first is tried until its own [`Policy`] is spent, then the second, and so on.
//! The order is followed strictly because the entries are not interchangeable:
//! the first is the cheapest and most reliable one a caller has, and the ones
//! after it are what that caller is willing to pay when the first will not
//! answer.
//!
//! # Failover is correct but it is not free
//!
//! The cached prompt prefix at the endpoint that failed is gone, so the first
//! request to the next one pays full price for the whole conversation. On a long
//! thread that is a real cost spike rather than a rounding error, which is why
//! every failover that lands is recorded as a `recovered` incident instead of
//! being absorbed silently. A failover that works looks exactly like success.
//!
//! # Same shape only
//!
//! The harness's request body is relayed unchanged, so every endpoint in one
//! list must accept the same request shape: several Anthropic keys, the same
//! provider listed twice, a self-hosted deployment of the same API. Only the
//! base URL and the credential differ per endpoint. Translating a conversation
//! between two providers' shapes, and rewriting tool call ids with it, is the
//! `LiteLLM` sidecar's job and is deliberately not done here.

use crate::proxy::upstream::Upstream;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::settings::v1::{ModelEndpoint, RetryPolicy};
use axum::http::HeaderMap;
use std::time::Duration;

/// How many times one endpoint is tried when it declares no policy.
///
/// The number the README publishes, so it is a contract rather than a tuning
/// knob. Ten attempts against a provider returning 429 spans several minutes of
/// backoff, which is the right shape for a rate limit that clears on its own.
const DEFAULT_ATTEMPTS: u32 = 10;

/// The first wait after a retryable answer, when an endpoint declares none.
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

/// The longest wait between two attempts, when an endpoint declares none.
///
/// Also the ceiling a `Retry-After` header is clamped to. A provider asking for
/// an hour is asking for longer than any turn wants to spend waiting.
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_mins(1);

/// Statuses retried rather than failed over, when an endpoint declares none.
///
/// 429 is a rate limit and 529 is Anthropic's overload. Both are the provider
/// saying "not now" rather than "not ever", which is exactly the distinction
/// between waiting and moving on.
const DEFAULT_RETRY_ON: [u16; 2] = [429, 529];

/// Statuses that mean the credential was rejected.
///
/// Never retried. A key that is wrong is wrong on the tenth attempt too, and a
/// second endpoint carrying different credentials is precisely what the list
/// exists for.
const AUTH_REJECTIONS: [u16; 2] = [401, 403];

/// The status that means the endpoint does not serve what was asked for.
const NOT_FOUND: u16 = 404;

/// The name an endpoint is recorded under when it declared none.
const UNNAMED: &str = "endpoint";

/// The name of the destination a thread with no declared endpoints reaches.
const AMBIENT: &str = "satellite default";

/// How hard one endpoint is tried before the next one is.
///
/// Resolved once per turn from the endpoint's [`RetryPolicy`], so the defaults
/// are written down here and nowhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
    retry_on: Vec<u16>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            attempts: DEFAULT_ATTEMPTS,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
            retry_on: DEFAULT_RETRY_ON.to_vec(),
        }
    }
}

impl Policy {
    /// Reads the policy an endpoint declared, defaulting whatever it left out.
    ///
    /// `max_attempts` counts requests rather than retries, so one attempt means
    /// one request and no retry. Zero is read as one for the same reason the
    /// contract says it "disables retries": a request is still made, because an
    /// endpoint nothing is ever sent to is not an endpoint.
    #[must_use]
    pub fn resolve(declared: Option<&RetryPolicy>) -> Self {
        let defaults = Self::default();

        let Some(declared) = declared else {
            return defaults;
        };

        Self {
            attempts: declared
                .max_attempts
                .map_or(defaults.attempts, |attempts| attempts.max(1)),
            initial_backoff: span(declared.initial_backoff.as_ref(), defaults.initial_backoff),
            max_backoff: span(declared.max_backoff.as_ref(), defaults.max_backoff),
            retry_on: if declared.retry_on_status.is_empty() {
                defaults.retry_on
            } else {
                // Narrowed here rather than at every comparison. A status is
                // three digits, and a caller that typed 42_900 meant 429 badly
                // rather than meaning a status that cannot exist.
                declared
                    .retry_on_status
                    .iter()
                    .filter_map(|status| u16::try_from(*status).ok())
                    .collect()
            },
        }
    }

    /// How many requests this endpoint may be sent.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether `status` is worth waiting out rather than failing over.
    #[must_use]
    pub fn retries(&self, status: u16) -> bool {
        self.retry_on.contains(&status)
    }

    /// How long to wait after the `attempt`th request, counting from one.
    ///
    /// Doubling from the initial wait and capped at the maximum, so a rate limit
    /// that clears quickly is met quickly and one that does not stops costing a
    /// request every five seconds.
    #[must_use]
    pub fn backoff_after(&self, attempt: u32) -> Duration {
        // Past 32 doublings the shift is undefined and the result is capped
        // anyway, so the exponent is clamped rather than left to wrap.
        let doublings = attempt.saturating_sub(1).min(u32::BITS - 1);

        self.initial_backoff
            .checked_mul(1_u32 << doublings)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }

    /// The wait an endpoint asked for, when it asked for one that makes sense.
    ///
    /// Only the delta-seconds form is read. The HTTP-date form is legal and no
    /// model provider sends it, and a date parsed wrong would produce a wait of
    /// hours rather than seconds.
    ///
    /// Clamped to [`Self::backoff_after`]'s own ceiling: a provider is better
    /// placed than we are to know when it will answer, and still not entitled to
    /// hold a turn for an hour.
    #[must_use]
    pub fn asked_wait(&self, headers: &HeaderMap) -> Option<Duration> {
        headers
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| Duration::from_secs(seconds).min(self.max_backoff))
    }
}

/// One declared span, or `fallback` when it is absent or unusable.
///
/// Zero and negative spans fall back for the same reason the timeout bounds
/// refuse them: a backoff of zero is a retry loop with no wait in it, which is
/// how a rate limit becomes a denial of service against the endpoint that
/// reported it.
fn span(declared: Option<&arsox_sdk::proto::common::v1::Duration>, fallback: Duration) -> Duration {
    declared
        .map(arsox_sdk::helpers::duration_to_nanos)
        .filter(|nanos| *nanos > 0)
        .and_then(|nanos| u64::try_from(nanos).ok())
        .map_or(fallback, Duration::from_nanos)
}

/// One place a turn's requests may go, and how hard to try it.
#[derive(Debug, Clone)]
pub struct Destination {
    /// The caller's label, which is what appears in `details.attempts` so two
    /// entries for the same provider can be told apart.
    pub name: String,

    /// Where the request goes and what credential it carries.
    pub upstream: Upstream,

    /// How hard this endpoint is tried before the next one is.
    pub policy: Policy,
}

/// Everywhere a turn's requests may go, in the order they are tried.
#[derive(Debug, Clone)]
pub struct Route {
    destinations: Vec<Destination>,
}

impl Route {
    /// Resolves the endpoints a thread declared into the order they are tried.
    ///
    /// A thread that declared none still goes through the proxy, using whatever
    /// credential the satellite itself holds. That is what keeps the chokepoint
    /// universal: an unconfigured thread must not be the one whose spending is
    /// unmeasured and whose credential sits in the agent's environment.
    #[must_use]
    pub fn resolve(endpoints: &[ModelEndpoint]) -> Self {
        if endpoints.is_empty() {
            return Self {
                destinations: vec![Destination {
                    name: AMBIENT.to_owned(),
                    upstream: Upstream::ambient(),
                    policy: Policy::default(),
                }],
            };
        }

        Self {
            destinations: endpoints
                .iter()
                .enumerate()
                .map(|(index, endpoint)| Destination {
                    name: label(endpoint, index),
                    upstream: Upstream::declared(endpoint),
                    policy: Policy::resolve(endpoint.retry.as_ref()),
                })
                .collect(),
        }
    }

    /// The destinations, in the order they are tried.
    #[must_use]
    pub fn destinations(&self) -> &[Destination] {
        &self.destinations
    }
}

/// What an endpoint is recorded as, whether or not it was named.
///
/// `name` is a bare string in the contract, so an empty one is what a caller
/// that never filled it in sends. An attempt list of three blank names would
/// answer "which endpoint failed" with nothing.
fn label(endpoint: &ModelEndpoint, index: usize) -> String {
    if endpoint.name.is_empty() {
        format!("{UNNAMED} {}", index + 1)
    } else {
        endpoint.name.clone()
    }
}

/// Why one endpoint was given up on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GaveUp {
    /// The endpoint rejected the satellite's credential.
    Unauthorized { status: u16 },

    /// A retryable status, still arriving once the policy was spent.
    RateLimited { status: u16 },

    /// Any other status the endpoint answered with.
    ///
    /// Not retried: a status outside the retry set is the endpoint saying
    /// something it will say again, and asking ten more times only spends ten
    /// more requests reaching it.
    Refused { status: u16 },

    /// The endpoint could not be reached at all.
    Unreachable { reason: String },

    /// Nothing came back inside the thread's request bound.
    TimedOut,
}

impl GaveUp {
    /// The contract code this failure is recorded under.
    ///
    /// The taxonomy names four per-endpoint conditions and routes everything
    /// else through the aggregate, whose whole job is to say that
    /// `details.attempts` holds the reason. So an endpoint that could not be
    /// reached is recorded as a timeout, which is the same fact from the
    /// harness's seat, and a status the taxonomy cannot name borrows the
    /// aggregate's code rather than being mislabeled as a rate limit. Both carry
    /// the real reason in their message and details. See the roadmap in
    /// `docs/llm-proxy.md`.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Unauthorized { .. } => ErrorCode::LlmEndpointUnauthorized,
            Self::RateLimited { .. } => ErrorCode::LlmEndpointRateLimited,
            Self::TimedOut | Self::Unreachable { .. } => ErrorCode::LlmEndpointTimeout,
            Self::Refused { status } if *status == NOT_FOUND => ErrorCode::LlmModelUnknown,
            Self::Refused { .. } => ErrorCode::LlmAllEndpointsExhausted,
        }
    }

    /// The status the endpoint answered with, when it answered at all.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        match self {
            Self::Unauthorized { status }
            | Self::RateLimited { status }
            | Self::Refused { status } => Some(*status),
            Self::Unreachable { .. } | Self::TimedOut => None,
        }
    }

    /// What to say about this failure, in a sentence a person can act on.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Unauthorized { status } => {
                format!("the endpoint rejected the satellite's credential with HTTP {status}")
            }
            Self::RateLimited { status } => {
                format!("the endpoint was still answering HTTP {status} when its retries ran out")
            }
            Self::Refused { status } => format!("the endpoint answered HTTP {status}"),
            Self::Unreachable { reason } => format!("the endpoint could not be reached: {reason}"),
            Self::TimedOut => {
                "the endpoint did not answer inside this thread's request bound".to_owned()
            }
        }
    }
}

/// How one endpoint was tried, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Position in the declared order, counting from one, which is how a caller
    /// reading `details.attempts` counts its own list.
    pub position: usize,

    /// The endpoint's label.
    pub name: String,

    /// How many requests were sent to it.
    pub made: u32,

    /// Why it was given up on.
    pub gave_up: GaveUp,
}

/// What a status outside an endpoint's retry set means for that endpoint.
///
/// Never a retry either way. A rejected credential is rejected on the tenth
/// attempt too, and any other status the policy does not retry is the endpoint
/// saying something it will say again.
#[must_use]
pub fn refusal(status: u16) -> GaveUp {
    if AUTH_REJECTIONS.contains(&status) {
        GaveUp::Unauthorized { status }
    } else {
        GaveUp::Refused { status }
    }
}

/// The evidence behind a failover, in the shape `Incident.details` takes.
///
/// `attempts` is the key the contract names for
/// `LLM_ALL_ENDPOINTS_EXHAUSTED`, and it carries the same shape whether the
/// failover recovered or ran out, so one reader handles both.
#[must_use]
pub fn details(attempts: &[Attempt], answered_by: Option<&str>) -> prost_types::Struct {
    let mut fields = std::collections::BTreeMap::new();

    fields.insert(
        "attempts".to_owned(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::ListValue(
                prost_types::ListValue {
                    values: attempts.iter().map(attempted).collect(),
                },
            )),
        },
    );

    if let Some(answered_by) = answered_by {
        fields.insert("answered_by".to_owned(), text(answered_by.to_owned()));
    }

    prost_types::Struct {
        fields: fields.into_iter().collect(),
    }
}

/// One entry in `details.attempts`.
fn attempted(attempt: &Attempt) -> prost_types::Value {
    let mut fields = std::collections::BTreeMap::new();

    fields.insert(
        "position".to_owned(),
        number(f64::from(
            u32::try_from(attempt.position).unwrap_or(u32::MAX),
        )),
    );
    fields.insert("endpoint".to_owned(), text(attempt.name.clone()));
    fields.insert("attempts".to_owned(), number(f64::from(attempt.made)));
    fields.insert(
        "code".to_owned(),
        text(attempt.gave_up.code().as_str_name().to_owned()),
    );
    fields.insert("reason".to_owned(), text(attempt.gave_up.reason()));

    if let Some(status) = attempt.gave_up.status() {
        fields.insert("status".to_owned(), number(f64::from(status)));
    }

    prost_types::Value {
        kind: Some(prost_types::value::Kind::StructValue(prost_types::Struct {
            fields: fields.into_iter().collect(),
        })),
    }
}

fn text(value: String) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(value)),
    }
}

fn number(value: f64) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::NumberValue(value)),
    }
}

/// How a turn waits between two attempts at the same endpoint.
///
/// Real time on a satellite. A test injects [`Waits::recorded`] instead, which
/// writes the span down and returns at once, so a policy measured in seconds is
/// exercised in milliseconds and the schedule it produced is asserted exactly
/// rather than approximated by a scaled-down copy.
///
/// Feature gated per M-TEST-UTIL. A recorded wait in a published image would be
/// a retry loop with no wait in it, which is the one thing a backoff exists to
/// prevent.
#[derive(Debug, Clone, Default)]
pub enum Waits {
    /// Sleeps, which is what a satellite does.
    #[default]
    Sleeping,

    /// Writes the span down and returns at once.
    #[cfg(feature = "test-util")]
    Recorded(Recorder),
}

impl Waits {
    /// A waiter that records what it would have slept, and the recorder that
    /// reads it back.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn recorded() -> (Self, Recorder) {
        let recorder = Recorder::default();

        (Self::Recorded(recorder.clone()), recorder)
    }

    /// Waits `span` out before the next attempt.
    pub async fn take(&self, span: Duration) {
        match self {
            Self::Sleeping => tokio::time::sleep(span).await,

            #[cfg(feature = "test-util")]
            Self::Recorded(recorder) => recorder.record(span),
        }
    }
}

/// The waits a turn would have taken, in order.
#[cfg(feature = "test-util")]
#[derive(Debug, Clone, Default)]
pub struct Recorder {
    // Shared ownership rather than a handle back, so the grant the proxy cloned
    // and the test that built it read the same list.
    taken: std::sync::Arc<std::sync::Mutex<Vec<Duration>>>,
}

#[cfg(feature = "test-util")]
impl Recorder {
    fn record(&self, span: Duration) {
        self.lock().push(span);
    }

    /// Every wait taken so far, in the order it was taken.
    #[must_use]
    pub fn taken(&self) -> Vec<Duration> {
        self.lock().clone()
    }

    /// A poisoned lock means another thread panicked while holding a list of
    /// durations. Recovering beats taking a test process down over it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Duration>> {
        self.taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::common::v1::Duration as ProtoDuration;

    fn seconds(seconds: i64) -> ProtoDuration {
        ProtoDuration { seconds, nanos: 0 }
    }

    #[test]
    fn an_endpoint_that_declared_no_policy_gets_the_documented_defaults() {
        // The README publishes these, so they are a contract rather than a
        // tuning knob nobody reads.
        let policy = Policy::resolve(None);

        assert_eq!(policy.attempts(), 10);
        assert!(policy.retries(429));
        assert!(policy.retries(529));
        assert!(!policy.retries(500), "only 429 and 529 by default");
        assert_eq!(policy.backoff_after(1), Duration::from_secs(5));
    }

    #[test]
    fn the_backoff_doubles_and_then_stops_at_the_ceiling() {
        let policy = Policy::resolve(None);

        assert_eq!(policy.backoff_after(1), Duration::from_secs(5));
        assert_eq!(policy.backoff_after(2), Duration::from_secs(10));
        assert_eq!(policy.backoff_after(3), Duration::from_secs(20));
        assert_eq!(policy.backoff_after(4), Duration::from_secs(40));
        assert_eq!(policy.backoff_after(5), Duration::from_mins(1));
        assert_eq!(
            policy.backoff_after(50),
            Duration::from_mins(1),
            "an exponent that would overflow still lands on the ceiling"
        );
    }

    #[test]
    fn a_declared_policy_wins_and_the_rest_still_default() {
        let policy = Policy::resolve(Some(&RetryPolicy {
            max_attempts: Some(2),
            initial_backoff: Some(seconds(1)),
            max_backoff: None,
            retry_on_status: vec![500, 503],
        }));

        assert_eq!(policy.attempts(), 2);
        assert_eq!(policy.backoff_after(1), Duration::from_secs(1));
        assert!(policy.retries(500), "a declared set replaces the default");
        assert!(!policy.retries(429));
        assert_eq!(
            policy.backoff_after(9),
            Duration::from_mins(1),
            "the ceiling it left out is still the documented one"
        );
    }

    #[test]
    fn zero_attempts_still_sends_one_request() {
        // "Disables retries and fails over immediately" is one request and no
        // second one. An endpoint nothing is ever sent to is not an endpoint.
        let policy = Policy::resolve(Some(&RetryPolicy {
            max_attempts: Some(0),
            ..RetryPolicy::default()
        }));

        assert_eq!(policy.attempts(), 1);
    }

    #[test]
    fn a_backoff_of_zero_falls_back_rather_than_hammering_the_endpoint() {
        // A retry loop with no wait in it is how a rate limit becomes a denial
        // of service against the endpoint that reported it.
        let policy = Policy::resolve(Some(&RetryPolicy {
            initial_backoff: Some(seconds(0)),
            max_backoff: Some(seconds(-1)),
            ..RetryPolicy::default()
        }));

        assert_eq!(policy.backoff_after(1), DEFAULT_INITIAL_BACKOFF);
        assert_eq!(policy.backoff_after(20), DEFAULT_MAX_BACKOFF);
    }

    fn with_retry_after(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::RETRY_AFTER,
            value.parse().expect("a header value"),
        );
        headers
    }

    #[test]
    fn an_endpoint_that_says_when_to_come_back_is_believed_within_reason() {
        let policy = Policy::resolve(None);

        assert_eq!(
            policy.asked_wait(&with_retry_after("2")),
            Some(Duration::from_secs(2)),
            "a provider knows better than our schedule does when it will answer"
        );
        assert_eq!(
            policy.asked_wait(&with_retry_after("3600")),
            Some(DEFAULT_MAX_BACKOFF),
            "and is still not entitled to hold a turn for an hour"
        );
        assert_eq!(
            policy.asked_wait(&with_retry_after("Wed, 21 Oct 2026 07:28:00 GMT")),
            None,
            "the date form is legal, unsent by model providers, and unread here"
        );
        assert_eq!(policy.asked_wait(&HeaderMap::new()), None);
    }

    fn endpoint(name: &str) -> ModelEndpoint {
        ModelEndpoint {
            name: name.to_owned(),
            model: "claude-opus-5".to_owned(),
            base_url: Some("https://one.example.com".to_owned()),
            auth: None,
            retry: None,
        }
    }

    #[test]
    fn endpoints_are_routed_in_the_order_they_were_declared() {
        // Strictly, and documented as strict. A list that reordered itself would
        // spend the expensive fallback first.
        let route = Route::resolve(&[endpoint("cheap"), endpoint("fallback")]);

        let names: Vec<&str> = route
            .destinations()
            .iter()
            .map(|destination| destination.name.as_str())
            .collect();

        assert_eq!(names, ["cheap", "fallback"]);
    }

    #[test]
    fn an_endpoint_that_was_never_named_is_still_identifiable() {
        // An attempt list of blank names answers "which one failed" with
        // nothing.
        let route = Route::resolve(&[endpoint(""), endpoint("")]);

        assert_eq!(route.destinations()[1].name, "endpoint 2");
    }

    #[test]
    fn a_thread_with_no_endpoints_still_routes_somewhere() {
        // The chokepoint has to be universal. A thread that declared nothing
        // must still traverse the proxy, or its spending is unmeasured and its
        // credential is back in the agent's environment.
        let route = Route::resolve(&[]);

        assert_eq!(route.destinations().len(), 1);
        assert_eq!(
            route.destinations()[0]
                .upstream
                .url_for("v1/messages", None),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn every_way_of_giving_up_records_a_code_and_a_reason() {
        // `details.attempts` is the only place the specific reason survives, so
        // every arm has to say something a person can act on.
        for gave_up in [
            GaveUp::Unauthorized { status: 401 },
            GaveUp::RateLimited { status: 429 },
            GaveUp::Refused { status: 500 },
            GaveUp::Unreachable {
                reason: "connection refused".to_owned(),
            },
            GaveUp::TimedOut,
        ] {
            assert_ne!(gave_up.code(), ErrorCode::Unspecified);
            assert!(!gave_up.reason().is_empty());
        }

        assert_eq!(
            GaveUp::Refused { status: 404 }.code(),
            ErrorCode::LlmModelUnknown,
            "an endpoint that does not serve the model has its own code"
        );
        assert_eq!(refusal(403), GaveUp::Unauthorized { status: 403 });
        assert_eq!(refusal(500), GaveUp::Refused { status: 500 });
    }

    #[test]
    fn the_attempt_list_names_every_endpoint_and_why_it_was_given_up_on() {
        let struct_value = details(
            &[
                Attempt {
                    position: 1,
                    name: "primary".to_owned(),
                    made: 3,
                    gave_up: GaveUp::RateLimited { status: 429 },
                },
                Attempt {
                    position: 2,
                    name: "backup".to_owned(),
                    made: 1,
                    gave_up: GaveUp::TimedOut,
                },
            ],
            Some("third"),
        );

        let Some(prost_types::value::Kind::ListValue(attempts)) = struct_value
            .fields
            .get("attempts")
            .and_then(|value| value.kind.clone())
        else {
            panic!("the attempts key is what the contract names");
        };

        assert_eq!(attempts.values.len(), 2);
        assert!(struct_value.fields.contains_key("answered_by"));
    }

    #[cfg(feature = "test-util")]
    #[tokio::test]
    async fn a_recorded_wait_is_written_down_rather_than_taken() {
        let (waits, recorder) = Waits::recorded();

        let started = std::time::Instant::now();
        waits.take(Duration::from_secs(30)).await;
        waits.take(Duration::from_mins(1)).await;

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a test asserting a schedule must not sit through it"
        );
        assert_eq!(
            recorder.taken(),
            [Duration::from_secs(30), Duration::from_mins(1)]
        );
    }
}
