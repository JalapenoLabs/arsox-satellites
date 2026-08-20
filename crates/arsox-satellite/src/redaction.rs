// Copyright © 2026 Jalapeno Labs

//! Masking a thread's secrets out of everything that leaves the satellite.
//!
//! A secret reaches the outside world through more channels than the obvious
//! one. The stream is the channel everybody thinks of and the least dangerous;
//! a credential written into an incident that outlives its thread, or echoed by
//! a checker into a turn result a host application logs, is the leak that
//! actually costs something. So the masking is done by one engine, at the
//! points where text crosses out of the satellite, rather than by a `replace`
//! somebody remembered to write at each of a dozen call sites.
//!
//! # What a thread's secrets are
//!
//! [`Redactor::for_thread`] takes them from the thread's settings:
//!
//! - every declared [`EnvVar`] whose `is_secret` resolves true, which is the
//!   same rule the spawn applies, absent included: absent means secret.
//! - every credential the caller handed the satellite: a repo's personal access
//!   token or SSH private key, the agents repo's, the GitHub and Jira tokens,
//!   and every LLM endpoint's key. **None of these carries an `is_secret` flag,
//!   so none of them is optional.** They are credentials by type rather than by
//!   declaration, and a thread cannot ask for one to be printed.
//!
//! # The modes and the two rules that override them
//!
//! [`mask`] implements the four reveal modes with the contract's own formulas,
//! and the two safety rules that outrank every one of them: a secret shorter
//! than [`SHORTEST_REVEALABLE`] characters is always anonymous, because there
//! is no safe prefix of a short secret, and a reveal that would expose more than
//! half the secret falls back to anonymous.
//!
//! # One pass, however many secrets
//!
//! A thread may declare many secrets and a busy thread emits events
//! continuously, so the scan is a single Aho-Corasick pass rather than one
//! `String::replace` per secret. Overlapping secrets resolve longest-first, so a
//! token that contains a shorter one is masked as the token rather than being
//! chopped in half by its own substring.
//!
//! **A thread with no secrets costs one branch.** The automaton is absent
//! rather than empty, and every entry point returns before it looks at the text.
//!
//! # What this module is not
//!
//! It is the scanning and masking engine and its wiring into the paths that
//! exist today. The root-owned `pre-push` hook that refuses a push carrying an
//! unredacted secret, and the `override_redaction` MCP tool that lets an agent
//! deliberately move the guardrail, are separate work. Neither changes what is
//! here: this masks what leaves, and the hook is the hard gate for git.

use aho_corasick::{AhoCorasick, MatchKind};
use arsox_sdk::proto::artifact::v1::Artifact;
use arsox_sdk::proto::error::v1::Error as ContractError;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::event::v1::{
    AgentMessage, AgentThinking, ArtifactCreated, BudgetWarning, CheckerResultEvent,
    IntegrationConflict, IntegrationLanded, IntegrationRequested, PlanDecided, PlanProposed,
    QuestionAnswered, QuestionAsked, RateLimitReported, RedactionOverridden, ServiceLog,
    ServiceStarted, StatisticsUpdated, TeamChat, TeamDirectMessage, TeamMemberDespawned,
    TeamMemberSpawned, ThreadEvent, ToolCompleted, ToolStarted, TurnCompleted, TurnStarted,
};
use arsox_sdk::proto::incident::v1::Incident;
use arsox_sdk::proto::interaction::v1::{
    Plan, Question, QuestionAnswer, QuestionOption, QuestionSet, question_answer::Answer,
};
use arsox_sdk::proto::settings::v1::{
    GitAuth, LlmAuth, Redaction, RedactionMode, StarCount, ThreadSettings, git_auth,
    llm_auth::Credential as LlmCredential, star_count::Style,
};
use arsox_sdk::proto::turn::v1::{
    ChangedFile, CheckerResult, IntegrationRecord, PullRequestWatchReport, StageOutcome,
    TeamMember, Turn, TurnResult,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

/// Stars a mask is made of when nothing else decides the count.
///
/// Six, from the contract. Fixed rather than mirroring the secret's length,
/// because mirroring leaks the length and a length is real information about a
/// credential.
const DEFAULT_STARS: usize = 6;

/// Most stars a mask is ever made of.
///
/// The contract accepts any positive count, and a mask is read by a human: a
/// count past this is a mistake or an attack, and either way it must not become
/// a multi-megabyte allocation per masked occurrence on a stream that never
/// stops. Clamping keeps the mask a mask.
const MAX_STARS: usize = 256;

/// Shortest secret any mode will reveal a character of.
///
/// There is no safe prefix of a short secret: eight characters revealed two at
/// a time is a quarter of the credential, and a four character one revealed at
/// all is most of it.
pub const SHORTEST_REVEALABLE: usize = 8;

/// Shortest value worth scanning for.
///
/// A declared variable inherits `is_secret` when the caller says nothing, so a
/// thread that sets `DEBUG=1` has declared a secret whose value is one
/// character. Indexing it would mask every `1` in every event, every path, and
/// every exit code, which destroys the stream the masking exists to protect
/// while hiding nothing anybody would call a credential.
///
/// So a value below this is not indexed, and [`Redactor::for_thread`] says so
/// with the key rather than dropping it silently. Three characters is not a
/// credential; the operator who typed one wants a flag, not a secret.
const MIN_SCANNABLE: usize = 4;

/// One thread's secrets, ready to be masked out of anything leaving the
/// satellite.
///
/// Cheap to clone: the automaton is shared, so a redactor travels with a turn
/// rather than being rebuilt for each thing it masks. A thread that declared
/// nothing holds no automaton at all.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Absent when the thread has no secrets, which is what makes the empty
    /// case one branch rather than an empty automaton walked per event.
    scanner: Option<Arc<Scanner>>,
}

/// Renders the count and never a secret.
///
/// Manual because a derived `Debug` on a type whose whole purpose is holding
/// credentials would print them into any log line that formatted a turn
/// context. Per M-PUBLIC-DEBUG.
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("secrets", &self.scanner.as_deref().map_or(0, Scanner::len))
            .finish()
    }
}

impl Redactor {
    /// A redactor with nothing to hide.
    ///
    /// The right value for text the satellite wrote itself, and for a thread
    /// that declared no credentials. Masking through it is a branch.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Builds the redactor for one thread from its settings.
    ///
    /// See the module docs for what counts as a secret. Warns once per declared
    /// variable too short to index, naming the key and never the value.
    #[must_use]
    pub fn for_thread(settings: &ThreadSettings) -> Self {
        let mut values = Vec::new();

        for variable in &settings.env {
            if !crate::harness::spawn::is_secret(variable) {
                continue;
            }

            let value = variable
                .value
                .as_ref()
                .and_then(|secret| secret.value.as_deref())
                .unwrap_or_default();

            if !value.is_empty() && value.chars().count() < MIN_SCANNABLE {
                tracing::warn!(
                    event.name = "redaction.secret.too_short",
                    env.key = variable.key,
                    redaction.minimum_length = MIN_SCANNABLE,
                    "{{env.key}} is marked secret but is shorter than \
                     {{redaction.minimum_length}} characters, so it is not masked: a value that \
                     short is not a credential and indexing it would mask ordinary text \
                     everywhere it appears",
                );
                continue;
            }

            values.push(value.to_owned());
        }

        // Credential by type rather than by declaration. None of these came
        // from an `EnvVar`, none carries an `is_secret` flag, and a thread has
        // no way to ask for one to be printed.
        for repo in &settings.repos {
            collect_git(&mut values, repo.auth.as_ref());
        }
        if let Some(agents) = settings.agents_repo.as_ref() {
            collect_git(&mut values, agents.auth.as_ref());
        }
        if let Some(github) = settings.github.as_ref() {
            push_secret(&mut values, github.token.as_ref());
        }
        if let Some(jira) = settings.jira.as_ref() {
            push_secret(&mut values, jira.token.as_ref());
        }
        for endpoint in &settings.models {
            collect_llm(&mut values, endpoint.auth.as_ref());
        }

        Self::for_values(values, settings.redaction.as_ref())
    }

    /// Builds a redactor over exactly these values.
    ///
    /// `settings` decides the mode and the star count; absent uses the
    /// contract's defaults, which are anonymous and six stars. Empty values and
    /// values below [`MIN_SCANNABLE`] are dropped, and duplicates collapse.
    #[must_use]
    pub fn for_values(values: Vec<String>, settings: Option<&Redaction>) -> Self {
        // The contract's own defaults rather than proto3's zero values, which
        // is the reading the rest of the settings surface uses.
        let documented = Redaction::default();
        let redaction = settings.unwrap_or(&documented);

        let mut secrets: Vec<String> = values
            .into_iter()
            .filter(|value| value.chars().count() >= MIN_SCANNABLE)
            .collect();

        // A secret declared twice is one pattern. Sorted first so the dedupe is
        // a single pass, and sorted again below by length for the fallback.
        secrets.sort_unstable();
        secrets.dedup();

        if secrets.is_empty() {
            return Self::none();
        }

        Self {
            scanner: Some(Arc::new(Scanner::new(secrets, redaction))),
        }
    }

    /// Whether this redactor would mask anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scanner.is_none()
    }

    /// Masks every secret in `text`, borrowing when there was nothing to mask.
    ///
    /// The borrow is the point: the overwhelmingly common case on a stream is
    /// text containing no secret, and it costs one scan and no allocation.
    #[must_use]
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.scanner.as_deref() {
            Some(scanner) => scanner.scan(text),
            None => Cow::Borrowed(text),
        }
    }

    /// Masks every secret in a whole stream event, payload included.
    pub fn redact_event(&self, event: &mut ThreadEvent) {
        if let Some(scanner) = self.scanner.as_deref() {
            event.payload.scrub(scanner);
        }
    }

    /// Masks every secret in a turn's result.
    ///
    /// Applied before the result is written and before it reaches the stream,
    /// so the durable copy and the live one cannot disagree about what a
    /// consumer was allowed to see.
    pub fn redact_turn_result(&self, result: &mut TurnResult) {
        if let Some(scanner) = self.scanner.as_deref() {
            result.scrub(scanner);
        }
    }

    /// Masks every secret in an incident, its message and its evidence alike.
    ///
    /// Incidents outlive the thread they describe, so this is the one masked
    /// thing that is still readable next week.
    pub fn redact_incident(&self, incident: &mut Incident) {
        if let Some(scanner) = self.scanner.as_deref() {
            incident.scrub(scanner);
        }
    }

    /// The incident as a consumer may see it, borrowing when nothing is hidden.
    ///
    /// For callers holding an incident they do not own. A thread with no
    /// secrets copies nothing, which matters because the same incident is
    /// frequently both streamed and recorded.
    #[must_use]
    pub fn redacted_incident<'a>(&self, incident: &'a Incident) -> Cow<'a, Incident> {
        if self.is_empty() {
            return Cow::Borrowed(incident);
        }

        let mut masked = incident.clone();
        self.redact_incident(&mut masked);

        Cow::Owned(masked)
    }
}

/// Collects a git credential's material, whichever kind it is.
fn collect_git(into: &mut Vec<String>, auth: Option<&GitAuth>) {
    match auth.and_then(|auth| auth.credential.as_ref()) {
        Some(git_auth::Credential::PersonalAccessToken(token)) => push_secret(into, Some(token)),
        Some(git_auth::Credential::SshKey(pair)) => push_secret(into, pair.private_key.as_ref()),
        None => {}
    }
}

/// Collects an LLM endpoint's credential, whichever kind it is.
fn collect_llm(into: &mut Vec<String>, auth: Option<&LlmAuth>) {
    match auth.and_then(|auth| auth.credential.as_ref()) {
        Some(LlmCredential::ApiKey(key) | LlmCredential::SubscriptionToken(key)) => {
            push_secret(into, Some(key));
        }
        Some(LlmCredential::Oauth(oauth)) => {
            push_secret(into, oauth.access_token.as_ref());
            push_secret(into, oauth.refresh_token.as_ref());
        }
        None => {}
    }
}

/// Pushes a credential's plaintext, when it has one.
fn push_secret(into: &mut Vec<String>, secret: Option<&arsox_sdk::proto::common::v1::Secret>) {
    if let Some(value) = secret.and_then(|secret| secret.value.as_deref())
        && !value.is_empty()
    {
        into.push(value.to_owned());
    }
}

/// Renders one secret as the mask that stands in for it.
///
/// `settings` carries the mode and the star count. Absent fields take the
/// contract's documented defaults rather than proto3's zero values, which is
/// the same reading the rest of the settings surface uses.
///
/// # Examples
///
/// ```
/// use arsox_satellite::redaction::mask;
/// use arsox_sdk::proto::settings::v1::{Redaction, RedactionMode};
///
/// let postfix = Redaction {
///     mode: RedactionMode::PostfixShown.into(),
///     ..Redaction::default()
/// };
///
/// assert_eq!(mask("sk_ant_12345", &postfix), "******45");
/// ```
#[must_use]
pub fn mask(secret: &str, settings: &Redaction) -> String {
    let characters: Vec<char> = secret.chars().collect();
    let mode = RedactionMode::try_from(settings.mode).unwrap_or(RedactionMode::Anonymous);
    let (head, tail) = reveal(mode, characters.len());

    let hidden = characters.len() - head - tail;
    let stars = star_count(settings.star_count.as_ref(), hidden);

    let mut masked = String::with_capacity(head + stars + tail);
    masked.extend(characters.iter().take(head));
    masked.extend(std::iter::repeat_n('*', stars));
    masked.extend(characters.iter().skip(characters.len() - tail));

    masked
}

/// How many characters a mode reveals at each end of a secret.
///
/// The two safety rules live here rather than at the call site, so no mode can
/// be added that quietly escapes them.
fn reveal(mode: RedactionMode, length: usize) -> (usize, usize) {
    // Rule one. There is no safe prefix of a short secret.
    if length < SHORTEST_REVEALABLE {
        return (0, 0);
    }

    let (head, tail) = match mode {
        // Unspecified fails safe. A caller who set no mode asked for redaction
        // and said nothing about how much of it, and the answer that cannot
        // leak is the one that reveals nothing.
        RedactionMode::Unspecified | RedactionMode::Anonymous => (0, 0),
        RedactionMode::PrefixShown => (edge_reveal(length), 0),
        RedactionMode::PostfixShown => (0, edge_reveal(length)),
        RedactionMode::HybridShown => {
            let each = hybrid_reveal(length);
            (each, each)
        }
    };

    // Rule two.
    if exposes_more_than_half(head + tail, length) {
        return (0, 0);
    }

    (head, tail)
}

/// Characters revealed by `PrefixShown` and `PostfixShown`.
///
/// `N = floor(min(8, 0.20 × length))`. Integer division by five is that floor
/// with no float anywhere near a security decision.
const fn edge_reveal(length: usize) -> usize {
    const CAP: usize = 8;

    if length / 5 < CAP { length / 5 } else { CAP }
}

/// Characters revealed at each end by `HybridShown`.
///
/// `N = max(1, floor(min(5, 0.10 × length)))`. The floor of one tenth is
/// integer division by ten, and the `max` is what keeps the mode meaningful for
/// a secret just past the short-secret rule.
const fn hybrid_reveal(length: usize) -> usize {
    const CAP: usize = 5;

    let tenth = if length / 10 < CAP { length / 10 } else { CAP };

    if tenth < 1 { 1 } else { tenth }
}

/// Whether revealing `revealed` of `length` characters shows more than half.
///
/// Written as a doubling rather than a division so a secret of odd length is
/// judged exactly: revealing 3 of 5 is more than half, revealing 2 is not.
///
/// **No mode reaches this today.** `PrefixShown` and `PostfixShown` reveal at
/// most a fifth, and `HybridShown` at most a fifth once the short-secret rule
/// has already taken everything below eight characters. It is checked anyway,
/// because it is the rule that has to hold when a fifth mode is added by
/// somebody who did not read this far.
const fn exposes_more_than_half(revealed: usize, length: usize) -> bool {
    revealed * 2 > length
}

/// How many stars stand in for the hidden part of a secret.
fn star_count(configured: Option<&StarCount>, hidden: usize) -> usize {
    let count = match configured.and_then(|count| count.style.as_ref()) {
        // Mirroring the length leaks the length, which is why it has to be
        // asked for rather than being what happens by default.
        Some(Style::Mirror(_mirrored)) => hidden,
        Some(Style::Fixed(fixed)) => usize::try_from(*fixed).unwrap_or(DEFAULT_STARS),
        // Absent, or a `StarCount` carrying no style at all.
        None => DEFAULT_STARS,
    };

    // A mask with no stars is invisible, and one with too many is not a mask.
    count.clamp(1, MAX_STARS)
}

/// A thread's secrets compiled into something that can walk text once.
struct Scanner {
    strategy: Strategy,

    /// The secrets themselves, longest first.
    ///
    /// Kept for [`Strategy::Sequential`], and kept in that order because
    /// longest-first replacement is what makes a secret containing another
    /// secret mask as itself.
    secrets: Vec<String>,

    /// The mask for each entry of `secrets`, at the same index.
    masks: Vec<String>,
}

/// How a scanner finds its secrets in text.
enum Strategy {
    /// One pass over the text whatever the number of secrets.
    Automaton(AhoCorasick),

    /// One pass per secret, longest first.
    ///
    /// Reached only when an automaton could not be built, which the library
    /// refuses for a pattern set orders of magnitude larger than a thread
    /// declares. It exists because the alternative to a slower scan is no scan,
    /// and no scan is a leak.
    Sequential,
}

impl Scanner {
    /// Compiles `secrets` and the mask each of them renders as.
    fn new(mut secrets: Vec<String>, settings: &Redaction) -> Self {
        // Longest first, so `Sequential` masks a containing secret before the
        // one it contains. The automaton gets the same guarantee from
        // `LeftmostLongest` regardless of order.
        secrets.sort_by(|left, right| {
            right
                .chars()
                .count()
                .cmp(&left.chars().count())
                .then_with(|| left.cmp(right))
        });

        let masks = secrets
            .iter()
            .map(|secret| mask(secret, settings))
            .collect();

        let strategy = match AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&secrets)
        {
            Ok(automaton) => Strategy::Automaton(automaton),
            Err(error) => {
                tracing::error!(
                    event.name = "redaction.automaton.unbuildable",
                    redaction.secrets = secrets.len(),
                    "could not compile {{redaction.secrets}} secrets into one pass, \
                     falling back to a scan per secret: {error}",
                );
                Strategy::Sequential
            }
        };

        Self {
            strategy,
            secrets,
            masks,
        }
    }

    fn len(&self) -> usize {
        self.secrets.len()
    }

    /// Replaces every secret in `text` with its mask.
    fn scan<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match &self.strategy {
            Strategy::Automaton(automaton) => self.scan_once(automaton, text),
            Strategy::Sequential => self.scan_per_secret(text),
        }
    }

    /// One walk of the text, whatever the number of secrets.
    fn scan_once<'a>(&self, automaton: &AhoCorasick, text: &'a str) -> Cow<'a, str> {
        let mut found = automaton.find_iter(text).peekable();

        // The common case on a busy stream: nothing to mask, nothing allocated.
        if found.peek().is_none() {
            return Cow::Borrowed(text);
        }

        let mut masked = String::with_capacity(text.len());
        let mut copied = 0;

        for occurrence in found {
            masked.push_str(&text[copied..occurrence.start()]);
            masked.push_str(&self.masks[occurrence.pattern()]);
            copied = occurrence.end();
        }
        masked.push_str(&text[copied..]);

        Cow::Owned(masked)
    }

    /// One walk of the text per secret, longest first.
    fn scan_per_secret<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut masked = Cow::Borrowed(text);

        for (secret, replacement) in self.secrets.iter().zip(&self.masks) {
            if masked.contains(secret.as_str()) {
                masked = Cow::Owned(masked.replace(secret.as_str(), replacement));
            }
        }

        masked
    }
}

/// Anything a scanner can walk and mask in place.
///
/// A trait rather than a pile of functions so that `Option`, `Vec`, and the
/// nested contract messages compose, and so the [`Payload`] arm below is an
/// exhaustive match: a payload added to the contract stops compiling here until
/// somebody says what its secrets are.
trait Scrub {
    fn scrub(&mut self, by: &Scanner);
}

impl Scrub for String {
    fn scrub(&mut self, by: &Scanner) {
        if let Cow::Owned(masked) = by.scan(self) {
            *self = masked;
        }
    }
}

impl<T: Scrub> Scrub for Option<T> {
    fn scrub(&mut self, by: &Scanner) {
        if let Some(inner) = self.as_mut() {
            inner.scrub(by);
        }
    }
}

impl<T: Scrub> Scrub for Vec<T> {
    fn scrub(&mut self, by: &Scanner) {
        for item in self.iter_mut() {
            item.scrub(by);
        }
    }
}

/// Masks the values of a `map<string, string>` and leaves the keys alone.
///
/// A key is a label the caller chose, and two keys masked into the same string
/// would silently collapse two entries into one. A value is where a caller
/// puts something worth hiding.
impl Scrub for HashMap<String, String> {
    fn scrub(&mut self, by: &Scanner) {
        for value in self.values_mut() {
            value.scrub(by);
        }
    }
}

impl Scrub for prost_types::Struct {
    fn scrub(&mut self, by: &Scanner) {
        for value in self.fields.values_mut() {
            value.scrub(by);
        }
    }
}

impl Scrub for prost_types::Value {
    fn scrub(&mut self, by: &Scanner) {
        use prost_types::value::Kind;

        match self.kind.as_mut() {
            Some(Kind::StringValue(text)) => text.scrub(by),
            Some(Kind::StructValue(nested)) => nested.scrub(by),
            Some(Kind::ListValue(list)) => list.values.scrub(by),
            Some(Kind::NullValue(_) | Kind::NumberValue(_) | Kind::BoolValue(_)) | None => {}
        }
    }
}

/// Every payload the contract can carry, and what in each of them is text.
///
/// The match is exhaustive on purpose. Identifiers the satellite or the harness
/// generates are left alone, because a UUID and a tool call id cannot carry a
/// credential and scanning them would buy nothing; everything an agent, a
/// command, or the caller wrote is scanned.
impl Scrub for Payload {
    fn scrub(&mut self, by: &Scanner) {
        match self {
            Self::AgentMessage(message) => message.scrub(by),
            Self::AgentThinking(thinking) => thinking.scrub(by),
            Self::ToolStarted(started) => started.scrub(by),
            Self::ToolCompleted(completed) => completed.scrub(by),
            Self::TeamMemberSpawned(spawned) => spawned.scrub(by),
            Self::TeamMemberDespawned(despawned) => despawned.scrub(by),
            Self::TeamChat(chat) => chat.scrub(by),
            Self::TeamDirectMessage(direct) => direct.scrub(by),
            Self::IntegrationRequested(requested) => requested.scrub(by),
            Self::IntegrationLanded(landed) => landed.scrub(by),
            Self::IntegrationConflict(conflict) => conflict.scrub(by),
            Self::CheckerResult(checker) => checker.scrub(by),
            Self::ServiceStarted(started) => started.scrub(by),
            Self::ServiceLog(log) => log.scrub(by),
            Self::BudgetWarning(warning) => warning.scrub(by),
            Self::PlanProposed(proposed) => proposed.scrub(by),
            Self::PlanDecided(decided) => decided.scrub(by),
            Self::QuestionAsked(asked) => asked.scrub(by),
            Self::QuestionAnswered(answered) => answered.scrub(by),
            Self::ArtifactCreated(created) => created.scrub(by),
            Self::RedactionOverridden(overridden) => overridden.scrub(by),
            Self::TurnStarted(started) => started.scrub(by),
            Self::TurnCompleted(completed) => completed.scrub(by),
            Self::StatisticsUpdated(statistics) => statistics.scrub(by),
            Self::RateLimitReported(reported) => reported.scrub(by),
            Self::Incident(incident) => incident.scrub(by),
        }
    }
}

impl Scrub for AgentMessage {
    fn scrub(&mut self, by: &Scanner) {
        self.text.scrub(by);
    }
}

impl Scrub for AgentThinking {
    fn scrub(&mut self, by: &Scanner) {
        self.text.scrub(by);
    }
}

/// A tool's input is where a shell command lands, credentials and all.
impl Scrub for ToolStarted {
    fn scrub(&mut self, by: &Scanner) {
        self.input.scrub(by);
    }
}

impl Scrub for ToolCompleted {
    fn scrub(&mut self, by: &Scanner) {
        self.output_preview.scrub(by);
    }
}

/// A role is a word the commander chose, and a member id is a UUID.
impl Scrub for TeamMemberSpawned {
    fn scrub(&mut self, _by: &Scanner) {}
}

impl Scrub for TeamMemberDespawned {
    fn scrub(&mut self, _by: &Scanner) {}
}

impl Scrub for TeamChat {
    fn scrub(&mut self, by: &Scanner) {
        self.text.scrub(by);
    }
}

impl Scrub for TeamDirectMessage {
    fn scrub(&mut self, by: &Scanner) {
        self.text.scrub(by);
    }
}

impl Scrub for IntegrationRequested {
    fn scrub(&mut self, by: &Scanner) {
        self.summary.scrub(by);
    }
}

impl Scrub for IntegrationLanded {
    fn scrub(&mut self, by: &Scanner) {
        self.summary.scrub(by);
    }
}

impl Scrub for IntegrationConflict {
    fn scrub(&mut self, by: &Scanner) {
        self.conflicting_paths.scrub(by);
    }
}

impl Scrub for CheckerResultEvent {
    fn scrub(&mut self, by: &Scanner) {
        self.result.scrub(by);
    }
}

impl Scrub for CheckerResult {
    fn scrub(&mut self, by: &Scanner) {
        self.command.scrub(by);
        self.output.scrub(by);
    }
}

/// A declared service's address can carry a credential in its userinfo.
impl Scrub for ServiceStarted {
    fn scrub(&mut self, by: &Scanner) {
        self.url.scrub(by);
    }
}

impl Scrub for ServiceLog {
    fn scrub(&mut self, by: &Scanner) {
        self.line.scrub(by);
    }
}

/// A ceiling and a percentage.
impl Scrub for BudgetWarning {
    fn scrub(&mut self, _by: &Scanner) {}
}

impl Scrub for PlanProposed {
    fn scrub(&mut self, by: &Scanner) {
        self.plan.scrub(by);
    }
}

impl Scrub for Plan {
    fn scrub(&mut self, by: &Scanner) {
        self.body.scrub(by);
    }
}

/// An id and a decision.
impl Scrub for PlanDecided {
    fn scrub(&mut self, _by: &Scanner) {}
}

impl Scrub for QuestionAsked {
    fn scrub(&mut self, by: &Scanner) {
        self.question_set.scrub(by);
    }
}

impl Scrub for QuestionSet {
    fn scrub(&mut self, by: &Scanner) {
        self.questions.scrub(by);
    }
}

impl Scrub for Question {
    fn scrub(&mut self, by: &Scanner) {
        self.title.scrub(by);
        self.detail.scrub(by);
        self.options.scrub(by);
    }
}

impl Scrub for QuestionOption {
    fn scrub(&mut self, by: &Scanner) {
        self.title.scrub(by);
        self.description.scrub(by);
    }
}

impl Scrub for QuestionAnswered {
    fn scrub(&mut self, by: &Scanner) {
        self.answers.scrub(by);
    }
}

impl Scrub for QuestionAnswer {
    fn scrub(&mut self, by: &Scanner) {
        match self.answer.as_mut() {
            // Freeform text is a human answering, and a human pasting a
            // credential into an answer is exactly the case this covers.
            Some(Answer::Text(text)) => text.scrub(by),
            // An option id is one the satellite put in the question set, and a
            // decline carries nothing at all.
            Some(Answer::OptionId(_) | Answer::Declined(_)) | None => {}
        }
    }
}

impl Scrub for ArtifactCreated {
    fn scrub(&mut self, by: &Scanner) {
        self.artifact.scrub(by);
    }
}

impl Scrub for Artifact {
    fn scrub(&mut self, by: &Scanner) {
        self.name.scrub(by);
        self.path.scrub(by);
    }
}

/// The justification an agent wrote for moving the guardrail, and the operation
/// it moved it for. `secret_key` is a key name rather than a value.
impl Scrub for RedactionOverridden {
    fn scrub(&mut self, by: &Scanner) {
        self.justification.scrub(by);
        self.operation.scrub(by);
    }
}

impl Scrub for TurnStarted {
    fn scrub(&mut self, by: &Scanner) {
        self.turn.scrub(by);
    }
}

impl Scrub for Turn {
    fn scrub(&mut self, by: &Scanner) {
        self.prompt.scrub(by);
        self.metadata.scrub(by);
    }
}

impl Scrub for TurnCompleted {
    fn scrub(&mut self, by: &Scanner) {
        self.result.scrub(by);
    }
}

impl Scrub for TurnResult {
    fn scrub(&mut self, by: &Scanner) {
        self.summary.scrub(by);
        self.error.scrub(by);
        self.members.scrub(by);
        self.changed_files.scrub(by);
        self.integrations.scrub(by);
        self.checker_results.scrub(by);
        self.artifacts.scrub(by);
        self.suggestions.scrub(by);
        self.stages.scrub(by);
        self.unanswered_questions.scrub(by);
        self.watch.scrub(by);
        self.metadata.scrub(by);
    }
}

impl Scrub for ContractError {
    fn scrub(&mut self, by: &Scanner) {
        self.message.scrub(by);
        self.details.scrub(by);
    }
}

/// An id and the role the commander gave it.
impl Scrub for TeamMember {
    fn scrub(&mut self, _by: &Scanner) {}
}

impl Scrub for ChangedFile {
    fn scrub(&mut self, by: &Scanner) {
        self.path.scrub(by);
    }
}

impl Scrub for IntegrationRecord {
    fn scrub(&mut self, by: &Scanner) {
        self.summary.scrub(by);
    }
}

impl Scrub for StageOutcome {
    fn scrub(&mut self, by: &Scanner) {
        self.reason.scrub(by);
    }
}

impl Scrub for PullRequestWatchReport {
    fn scrub(&mut self, by: &Scanner) {
        self.termination_reason.scrub(by);
    }
}

impl Scrub for arsox_sdk::proto::suggestion::v1::SuggestionReport {
    fn scrub(&mut self, by: &Scanner) {
        self.tech_debt.scrub(by);
        self.improvements.scrub(by);
        self.setup_script.scrub(by);
    }
}

impl Scrub for arsox_sdk::proto::suggestion::v1::Suggestion {
    fn scrub(&mut self, by: &Scanner) {
        self.title.scrub(by);
        self.body.scrub(by);
        self.locations.scrub(by);
    }
}

impl Scrub for arsox_sdk::proto::suggestion::v1::SourceLocation {
    fn scrub(&mut self, by: &Scanner) {
        self.path.scrub(by);
    }
}

impl Scrub for arsox_sdk::proto::suggestion::v1::SetupScriptSuggestion {
    fn scrub(&mut self, by: &Scanner) {
        self.title.scrub(by);
        self.body.scrub(by);
        self.evidence.scrub(by);
        self.proposed_setup_commands.scrub(by);
    }
}

impl Scrub for arsox_sdk::proto::suggestion::v1::CommandEvidence {
    fn scrub(&mut self, by: &Scanner) {
        self.command.scrub(by);
        self.output.scrub(by);
    }
}

/// Token counts and model names.
impl Scrub for StatisticsUpdated {
    fn scrub(&mut self, _by: &Scanner) {}
}

/// The endpoint's name is the one the caller gave it in `ModelEndpoint`.
impl Scrub for RateLimitReported {
    fn scrub(&mut self, by: &Scanner) {
        self.endpoint_name.scrub(by);
    }
}

impl Scrub for Incident {
    fn scrub(&mut self, by: &Scanner) {
        self.message.scrub(by);
        self.details.scrub(by);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::common::v1::Secret;
    use arsox_sdk::proto::settings::v1::{EnvVar, MirrorLength, Repo};

    /// The redaction settings for one mode, everything else at its default.
    fn with_mode(mode: RedactionMode) -> Redaction {
        Redaction {
            mode: mode.into(),
            ..Redaction::default()
        }
    }

    /// The README's worked example, which is the contract these formulas owe.
    const WORKED: &str = "sk_ant_12345";

    #[test]
    fn the_four_modes_render_the_documented_example() {
        // L = 12. Prefix and postfix reveal floor(min(8, 0.20 x 12)) = 2, and
        // hybrid reveals max(1, floor(min(5, 0.10 x 12))) = 1 at each end.
        assert_eq!(WORKED.chars().count(), 12);

        assert_eq!(mask(WORKED, &with_mode(RedactionMode::Anonymous)), "******");
        assert_eq!(
            mask(WORKED, &with_mode(RedactionMode::PrefixShown)),
            "sk******"
        );
        assert_eq!(
            mask(WORKED, &with_mode(RedactionMode::PostfixShown)),
            "******45"
        );
        assert_eq!(
            mask(WORKED, &with_mode(RedactionMode::HybridShown)),
            "s******5"
        );
    }

    #[test]
    fn an_unset_mode_reveals_nothing() {
        // A caller who set no mode asked for redaction and said nothing about
        // how much of it. Reading the zero value as a reveal would turn silence
        // into a disclosure.
        assert_eq!(mask(WORKED, &Redaction::default()), "******");
        assert_eq!(
            mask(WORKED, &with_mode(RedactionMode::Unspecified)),
            "******"
        );
    }

    #[test]
    fn a_secret_shorter_than_eight_characters_is_always_anonymous() {
        // The first safety rule. There is no safe prefix of a short secret, and
        // it outranks every mode rather than being one mode's special case.
        for mode in [
            RedactionMode::PrefixShown,
            RedactionMode::PostfixShown,
            RedactionMode::HybridShown,
        ] {
            assert_eq!(mask("abcdefg", &with_mode(mode)), "******", "{mode:?}");
            assert_eq!(mask("a", &with_mode(mode)), "******", "{mode:?}");
        }

        // Eight is the first length any mode reveals a character of.
        assert_eq!(
            mask("abcdefgh", &with_mode(RedactionMode::PrefixShown)),
            "a******"
        );
    }

    #[test]
    fn a_reveal_past_half_the_secret_falls_back_to_anonymous() {
        // The second safety rule, checked directly: no mode in the contract can
        // reach it, because the widest of them reveals a fifth. It is what has
        // to hold when a fifth mode is added.
        assert!(exposes_more_than_half(3, 5));
        assert!(exposes_more_than_half(5, 8));
        assert!(!exposes_more_than_half(2, 5));
        assert!(!exposes_more_than_half(4, 8));

        // And every mode the contract does define stays under it, at every
        // length the first rule lets through.
        for length in SHORTEST_REVEALABLE..512 {
            assert!(!exposes_more_than_half(edge_reveal(length), length));
            assert!(!exposes_more_than_half(2 * hybrid_reveal(length), length));
        }
    }

    #[test]
    fn the_star_count_follows_the_documented_table() {
        let starred = |style: Option<Style>| Redaction {
            mode: RedactionMode::PostfixShown.into(),
            star_count: Some(StarCount { style }),
            ..Redaction::default()
        };

        // Absent uses six.
        assert_eq!(mask(WORKED, &starred(None)), "******45");
        // A positive integer is honoured exactly.
        assert_eq!(mask(WORKED, &starred(Some(Style::Fixed(3)))), "***45");
        // Zero clamps to one, because a mask with no stars is invisible.
        assert_eq!(mask(WORKED, &starred(Some(Style::Fixed(0)))), "*45");
        // Mirroring gives one star per hidden character, and leaks the length,
        // which is why it has to be asked for.
        assert_eq!(
            mask(WORKED, &starred(Some(Style::Mirror(MirrorLength {})))),
            "**********45"
        );
        // A count nobody could read is clamped rather than allocated.
        assert_eq!(
            mask(WORKED, &starred(Some(Style::Fixed(u32::MAX))))
                .chars()
                .count(),
            MAX_STARS + 2
        );
    }

    #[test]
    fn mirroring_an_anonymous_secret_still_shows_a_mask() {
        let mirrored = Redaction {
            mode: RedactionMode::Anonymous.into(),
            star_count: Some(StarCount {
                style: Some(Style::Mirror(MirrorLength {})),
            }),
            ..Redaction::default()
        };

        assert_eq!(mask("abcdefgh", &mirrored), "********");
    }

    /// A redactor over exactly these values, anonymous by default.
    fn over(values: &[&str]) -> Redactor {
        Redactor::for_values(
            values.iter().map(|value| (*value).to_owned()).collect(),
            None,
        )
    }

    #[test]
    fn a_thread_with_no_secrets_borrows_rather_than_allocating() {
        let redactor = Redactor::none();

        assert!(redactor.is_empty());
        assert!(matches!(
            redactor.redact("nothing to hide here"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn text_with_no_secret_in_it_is_borrowed() {
        // The common case on a busy stream. Allocating a copy of every event
        // that happens not to contain a credential is the cost this avoids.
        let redactor = over(&["ghp_the_real_token"]);

        assert!(matches!(
            redactor.redact("the build passed"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn every_occurrence_of_a_secret_is_masked() {
        let redactor = over(&["ghp_the_real_token"]);

        let masked = redactor.redact("sent ghp_the_real_token twice: ghp_the_real_token");

        assert_eq!(masked, "sent ****** twice: ******");
    }

    #[test]
    fn the_longest_secret_wins_where_two_overlap() {
        // A token that contains a shorter secret has to mask as the token. The
        // other reading chops the long one in half and leaves its tail in the
        // clear, which is the worst of both.
        let redactor = over(&["token", "ghp_token_suffix"]);

        let masked = redactor.redact("saw ghp_token_suffix and token");

        assert_eq!(masked, "saw ****** and ******");
        assert!(!masked.contains("_suffix"));
    }

    #[test]
    fn a_scan_per_secret_reaches_the_same_answer() {
        // The fallback for an automaton that would not build. It is longest
        // first for the same reason `LeftmostLongest` is.
        let scanner = Scanner {
            strategy: Strategy::Sequential,
            secrets: vec!["ghp_token_suffix".to_owned(), "token".to_owned()],
            masks: vec!["******".to_owned(), "******".to_owned()],
        };

        assert_eq!(
            scanner.scan("saw ghp_token_suffix and token"),
            "saw ****** and ******"
        );
        assert!(matches!(scanner.scan("nothing here"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_value_too_short_to_be_a_credential_is_never_indexed() {
        // A thread setting `DEBUG=1` has declared a secret by omission.
        // Indexing it would mask every `1` in every event and hide nothing
        // anybody would call a credential.
        let redactor = over(&["1", "ab", "abc"]);

        assert!(redactor.is_empty());
        assert_eq!(redactor.redact("exit code 1 in abc"), "exit code 1 in abc");
    }

    #[test]
    fn a_variable_that_did_not_say_is_treated_as_a_secret() {
        // The same rule the spawn applies. Reading absence as public would turn
        // "the caller said nothing" into "the caller said this is printable".
        let declared = |key: &str, value: &str, is_secret: Option<bool>| EnvVar {
            key: key.to_owned(),
            value: Some(Secret {
                value: Some(value.to_owned()),
                display: None,
            }),
            is_secret,
        };

        let redactor = Redactor::for_thread(&ThreadSettings {
            env: vec![
                declared("SAID_NOTHING", "value-said-nothing", None),
                declared("SAID_PUBLIC", "value-said-public", Some(false)),
                declared("SAID_SECRET", "value-said-secret", Some(true)),
            ],
            ..ThreadSettings::default()
        });

        let masked = redactor.redact("value-said-nothing value-said-public value-said-secret");

        assert!(!masked.contains("value-said-nothing"));
        assert!(!masked.contains("value-said-secret"));
        assert!(
            masked.contains("value-said-public"),
            "a value the caller marked public reads plainly: {masked}"
        );
    }

    #[test]
    fn repo_credentials_are_masked_whatever_the_thread_declared() {
        // They never came from an `EnvVar` and carry no `is_secret` flag, so
        // there is no setting that turns them off. A thread that could print
        // its own deploy key would be a thread that could publish it.
        let redactor = Redactor::for_thread(&ThreadSettings {
            repos: vec![Repo {
                name: "api".to_owned(),
                url: "https://github.com/acme/api.git".to_owned(),
                auth: Some(GitAuth {
                    credential: Some(git_auth::Credential::PersonalAccessToken(Secret {
                        value: Some("ghp_the_real_token".to_owned()),
                        display: None,
                    })),
                }),
                ..Repo::default()
            }],
            ..ThreadSettings::default()
        });

        assert!(!redactor.is_empty());
        assert_eq!(
            redactor.redact("remote: rejected ghp_the_real_token"),
            "remote: rejected ******"
        );
    }

    #[test]
    fn an_ssh_private_key_is_masked_and_its_public_half_is_not() {
        use arsox_sdk::proto::settings::v1::SshKeyPair;

        let redactor = Redactor::for_thread(&ThreadSettings {
            repos: vec![Repo {
                auth: Some(GitAuth {
                    credential: Some(git_auth::Credential::SshKey(SshKeyPair {
                        private_key: Some(Secret {
                            value: Some("PRIVATE-KEY-MATERIAL".to_owned()),
                            display: None,
                        }),
                        public_key: Some("ssh-ed25519 AAAA".to_owned()),
                    })),
                }),
                ..Repo::default()
            }],
            ..ThreadSettings::default()
        });

        let masked = redactor.redact("used PRIVATE-KEY-MATERIAL for ssh-ed25519 AAAA");

        assert!(!masked.contains("PRIVATE-KEY-MATERIAL"));
        assert!(
            masked.contains("ssh-ed25519 AAAA"),
            "a public key is not a credential: {masked}"
        );
    }

    #[test]
    fn the_debug_rendering_counts_secrets_and_prints_none_of_them() {
        // Per M-PUBLIC-DEBUG: a type whose whole purpose is holding credentials
        // must not put them into any log line that formats a turn context.
        let rendered = format!("{:?}", over(&["ghp_the_real_token"]));

        assert!(!rendered.contains("ghp_the_real_token"), "{rendered}");
        assert!(rendered.contains('1'), "{rendered}");
    }

    #[test]
    fn an_event_payload_is_masked_wherever_its_text_lives() {
        let redactor = over(&["ghp_the_real_token"]);

        let mut event = ThreadEvent {
            payload: Some(Payload::AgentMessage(AgentMessage {
                author: None,
                text: "I used ghp_the_real_token".to_owned(),
            })),
            ..ThreadEvent::default()
        };
        redactor.redact_event(&mut event);

        let Some(Payload::AgentMessage(message)) = event.payload else {
            panic!("the payload should have survived masking");
        };
        assert_eq!(message.text, "I used ******");
    }

    #[test]
    fn a_checker_that_echoed_a_secret_is_masked_in_the_turn_result() {
        let redactor = over(&["ghp_the_real_token"]);

        let mut result = TurnResult {
            summary: "pushed with ghp_the_real_token".to_owned(),
            checker_results: vec![CheckerResult {
                command: "echo $GH_TOKEN".to_owned(),
                exit_code: 1,
                output: "ghp_the_real_token is not valid".to_owned(),
                skipped_by_commander: false,
            }],
            ..TurnResult::default()
        };
        redactor.redact_turn_result(&mut result);

        assert_eq!(result.summary, "pushed with ******");
        assert_eq!(result.checker_results[0].output, "****** is not valid");
    }

    #[test]
    fn an_incident_is_masked_in_its_message_and_in_its_evidence() {
        // Incidents outlive the thread they describe, so an unmasked one is a
        // credential in a database row long after the workspace is gone.
        let redactor = over(&["ghp_the_real_token"]);

        let mut incident = Incident {
            message: "setup command failed".to_owned(),
            details: Some(prost_types::Struct {
                fields: [(
                    "output".to_owned(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(
                            "fatal: bad credentials ghp_the_real_token".to_owned(),
                        )),
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..Incident::default()
        };
        redactor.redact_incident(&mut incident);

        let Some(prost_types::value::Kind::StringValue(output)) = incident
            .details
            .and_then(|details| details.fields.get("output").cloned())
            .and_then(|value| value.kind)
        else {
            panic!("the evidence should have survived masking");
        };

        assert_eq!(output, "fatal: bad credentials ******");
    }

    #[test]
    fn nested_payloads_are_reached_rather_than_only_the_top_level_ones() {
        // A turn result travels inside a `turn.completed` event, and a question
        // set inside both. A visitor that stopped at the envelope would mask
        // the shallow cases and miss every one that matters.
        let redactor = over(&["ghp_the_real_token"]);

        let mut event = ThreadEvent {
            payload: Some(Payload::TurnCompleted(TurnCompleted {
                result: Some(TurnResult {
                    unanswered_questions: vec![QuestionSet {
                        questions: vec![Question {
                            title: "is ghp_the_real_token right?".to_owned(),
                            options: vec![QuestionOption {
                                description: Some("try ghp_the_real_token".to_owned()),
                                ..QuestionOption::default()
                            }],
                            ..Question::default()
                        }],
                        ..QuestionSet::default()
                    }],
                    ..TurnResult::default()
                }),
            })),
            ..ThreadEvent::default()
        };
        redactor.redact_event(&mut event);

        let rendered = format!("{event:?}");
        assert!(!rendered.contains("ghp_the_real_token"), "{rendered}");
    }
}
