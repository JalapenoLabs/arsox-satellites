// Copyright © 2026 Jalapeno Labs

//! Where a model request goes, and what credential it carries.

use arsox_sdk::proto::settings::v1::llm_auth::Credential;
use arsox_sdk::proto::settings::v1::{CredentialPresentation, LlmAuth, ModelEndpoint};

/// The provider's own base URL, used when a thread declares no endpoint.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Credentials the satellite falls back to, in the order they are consulted.
///
/// Read from the satellite's own environment at boot and never placed in an
/// agent's. `AUTH_TOKEN` is a bearer token and `API_KEY` is a key header, which
/// is why the two are not interchangeable.
const AMBIENT_BEARER: &str = "ANTHROPIC_AUTH_TOKEN";
const AMBIENT_API_KEY: &str = "ANTHROPIC_API_KEY";

/// How a credential is presented to the provider, and which one.
///
/// The contract's [`CredentialPresentation`] names the same two headers without
/// the credential in them, because a caller declares where its key goes and the
/// satellite is what pairs that with the key itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Presentation {
    /// `x-api-key`, which is how a plain API key is sent.
    ApiKey(String),

    /// `Authorization: Bearer`, which is how subscription and OAuth tokens are.
    Bearer(String),

    /// The request goes out bare.
    ///
    /// Reached when a thread declares no endpoint and the satellite holds no
    /// ambient credential either. The upstream rejects it and the turn fails
    /// with the provider's own message, which is more useful than the satellite
    /// inventing one about configuration it cannot see.
    None,
}

/// The header an endpoint declared its credential goes in, if it declared one.
///
/// A declaration wins over inference, and it wins for every kind of credential
/// rather than for API keys alone. The field says how this endpoint's credential
/// is sent, so one rule with no exceptions is what lets a caller answer it
/// without first working out which arm of the oneof the satellite will read.
///
/// A value this satellite does not recognize is read as no declaration. Enum
/// values are additive within a proto major, so an older satellite will meet
/// ones it has never heard of, and inferring is a better answer than refusing a
/// turn over a field set for a newer one.
fn declared_header(auth: &LlmAuth) -> Option<fn(String) -> Presentation> {
    match auth
        .presentation
        .and_then(|declared| CredentialPresentation::try_from(declared).ok())
    {
        Some(CredentialPresentation::ApiKeyHeader) => Some(Presentation::ApiKey),
        Some(CredentialPresentation::Bearer) => Some(Presentation::Bearer),
        Some(CredentialPresentation::Unspecified) | None => None,
    }
}

/// Whether this destination takes an API key as a bearer token.
///
/// The proxy relays the harness's request body opaquely and swaps the
/// credential, so a Codex thread reaches an OpenAI endpoint on the same path the
/// body already names. What does not carry across is the header: Anthropic reads
/// an API key from `x-api-key` and OpenAI reads one from `Authorization: Bearer`.
/// Sending an OpenAI key in `x-api-key` is refused in a way that reads as a bad
/// credential rather than as a mis-shaped request, which is the most expensive
/// possible way to be wrong about a header.
///
/// Decided from the destination rather than from the key, because a key is a
/// string of characters and guessing a vendor from its prefix is the kind of
/// rule that breaks the first time a vendor changes one.
///
/// This is inference, and it is only consulted when the endpoint declared
/// nothing. A self-hosted OpenAI-compatible deployment lives on a host that
/// matches nobody's, and [`declared_header`] is how it says so.
fn api_key_is_a_bearer(base_url: &str) -> bool {
    let Some(host) = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    else {
        return false;
    };

    // Suffix matched on a label boundary rather than by `contains`, so
    // `openai.com.example.invalid` is not read as OpenAI's.
    host == "openai.com" || host.ends_with(".openai.com")
}

/// One resolved destination for a turn's model requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    base_url: String,
    presentation: Presentation,
}

impl Upstream {
    /// Where one declared endpoint's requests go, and what they carry.
    ///
    /// One destination rather than a choice between several. Which destinations
    /// exist and in what order they are tried is [`Route`]'s to decide.
    ///
    /// [`Route`]: crate::proxy::failover::Route
    #[must_use]
    pub fn declared(endpoint: &ModelEndpoint) -> Self {
        let base_url = endpoint
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());

        let auth = endpoint.auth.as_ref();
        let declared = auth.and_then(declared_header);

        // An API key is the one credential whose inferred header depends on
        // where it is going. See [`api_key_is_a_bearer`]. A token is a bearer
        // wherever it goes, so inference has nothing to decide for it.
        let as_key = declared.unwrap_or(if api_key_is_a_bearer(&base_url) {
            Presentation::Bearer
        } else {
            Presentation::ApiKey
        });
        let as_token = declared.unwrap_or(Presentation::Bearer);

        let presentation = match auth.and_then(|auth| auth.credential.as_ref()) {
            Some(Credential::ApiKey(secret)) => {
                secret.value.clone().map_or(Presentation::None, as_key)
            }
            Some(Credential::SubscriptionToken(secret)) => {
                secret.value.clone().map_or(Presentation::None, as_token)
            }
            Some(Credential::Oauth(oauth)) => oauth
                .access_token
                .as_ref()
                .and_then(|secret| secret.value.clone())
                .map_or(Presentation::None, as_token),
            None => Presentation::None,
        };

        Self {
            base_url,
            presentation,
        }
    }

    /// Where a thread that declared no endpoint goes.
    ///
    /// Still through the proxy, using whatever credential the satellite itself
    /// holds. That is what keeps the chokepoint universal: an unconfigured
    /// thread must not be the one whose spending is unmeasured and whose
    /// credential sits in the agent's environment.
    #[must_use]
    pub fn ambient() -> Self {
        let presentation = std::env::var(AMBIENT_BEARER)
            .ok()
            .filter(|token| !token.is_empty())
            .map_or_else(
                || {
                    std::env::var(AMBIENT_API_KEY)
                        .ok()
                        .filter(|key| !key.is_empty())
                        .map_or(Presentation::None, Presentation::ApiKey)
                },
                Presentation::Bearer,
            );

        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            presentation,
        }
    }

    /// The upstream URL for a path the agent asked for.
    #[must_use]
    pub fn url_for(&self, path: &str, query: Option<&str>) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');

        match query {
            Some(query) if !query.is_empty() => format!("{base}/{path}?{query}"),
            _absent => format!("{base}/{path}"),
        }
    }

    /// The credential headers to attach on the way out.
    #[must_use]
    pub fn credential_headers(&self) -> Vec<(&'static str, String)> {
        match &self.presentation {
            Presentation::ApiKey(key) => vec![("x-api-key", key.clone())],
            Presentation::Bearer(token) => vec![("authorization", format!("Bearer {token}"))],
            Presentation::None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::common::v1::Secret;
    use arsox_sdk::proto::settings::v1::OAuthCredential;

    fn endpoint_with(credential: Credential) -> ModelEndpoint {
        ModelEndpoint {
            name: "primary".to_owned(),
            model: "claude-opus-5".to_owned(),
            base_url: None,
            auth: Some(LlmAuth {
                credential: Some(credential),
                presentation: None,
            }),
            retry: None,
        }
    }

    /// `endpoint` with its credential declared as going in `presentation`.
    fn declaring(
        mut endpoint: ModelEndpoint,
        presentation: CredentialPresentation,
    ) -> ModelEndpoint {
        if let Some(auth) = endpoint.auth.as_mut() {
            auth.presentation = Some(presentation.into());
        }

        endpoint
    }

    fn secret(value: &str) -> Secret {
        Secret {
            value: Some(value.to_owned()),
            display: None,
        }
    }

    #[test]
    fn an_api_key_is_sent_as_a_key_and_a_token_as_a_bearer() {
        // Not interchangeable. Sending a subscription token in `x-api-key` is
        // rejected by the provider, and the failure looks like a bad credential
        // rather than a mis-shaped request.
        let key = Upstream::declared(&endpoint_with(Credential::ApiKey(secret("sk-ant-123"))));
        assert_eq!(
            key.credential_headers(),
            vec![("x-api-key", "sk-ant-123".to_owned())]
        );

        let token = Upstream::declared(&endpoint_with(Credential::SubscriptionToken(secret(
            "sess-abc",
        ))));
        assert_eq!(
            token.credential_headers(),
            vec![("authorization", "Bearer sess-abc".to_owned())]
        );
    }

    #[test]
    fn an_oauth_credential_presents_its_access_token() {
        let oauth = Upstream::declared(&endpoint_with(Credential::Oauth(OAuthCredential {
            access_token: Some(secret("access-1")),
            refresh_token: Some(secret("refresh-1")),
            expires_at: None,
        })));

        assert_eq!(
            oauth.credential_headers(),
            vec![("authorization", "Bearer access-1".to_owned())],
            "the refresh token is never presented to the provider"
        );
    }

    #[test]
    fn an_api_key_for_openai_is_presented_the_way_openai_reads_one() {
        // The proxy relays the body opaquely, so an OpenAI-shaped request from a
        // Codex thread reaches an OpenAI endpoint unchanged. The header does not
        // carry across: this key in `x-api-key` is refused in a way that reads
        // as a bad credential rather than as a mis-shaped request.
        let mut endpoint = endpoint_with(Credential::ApiKey(secret("sk-proj-123")));
        endpoint.base_url = Some("https://api.openai.com".to_owned());

        assert_eq!(
            Upstream::declared(&endpoint).credential_headers(),
            vec![("authorization", "Bearer sk-proj-123".to_owned())]
        );

        assert_eq!(
            Upstream::declared(&declaring(endpoint, CredentialPresentation::Unspecified))
                .credential_headers(),
            vec![("authorization", "Bearer sk-proj-123".to_owned())],
            "unspecified is what every endpoint written before this field says, \
             so it has to mean the inference that was there before it"
        );
    }

    #[test]
    fn a_declared_header_wins_over_what_the_host_would_have_implied() {
        // The whole reason the field exists. A self-hosted OpenAI-compatible
        // deployment lives on a host that matches nobody's, so inference reaches
        // `x-api-key` and the endpoint rejects the request in a way that reads
        // as a bad key rather than as a mis-shaped request.
        let mut self_hosted = endpoint_with(Credential::ApiKey(secret("local-key")));
        self_hosted.base_url = Some("https://llm.internal.example.com/v1".to_owned());

        assert_eq!(
            Upstream::declared(&declaring(self_hosted, CredentialPresentation::Bearer))
                .credential_headers(),
            vec![("authorization", "Bearer local-key".to_owned())]
        );

        // And the other direction, so this is a declaration rather than a way of
        // nudging inference: an OpenAI host would have implied a bearer.
        let mut openai = endpoint_with(Credential::ApiKey(secret("sk-proj-123")));
        openai.base_url = Some("https://api.openai.com".to_owned());

        assert_eq!(
            Upstream::declared(&declaring(openai, CredentialPresentation::ApiKeyHeader))
                .credential_headers(),
            vec![("x-api-key", "sk-proj-123".to_owned())]
        );
    }

    #[test]
    fn a_declaration_covers_every_kind_of_credential_it_could_apply_to() {
        // The field says how this endpoint's credential is sent. Honouring it
        // for API keys only would make the answer depend on which arm of the
        // oneof the caller filled in, which is exactly the guessing the
        // declaration exists to end.
        let token = endpoint_with(Credential::SubscriptionToken(secret("sess-abc")));

        assert_eq!(
            Upstream::declared(&declaring(token, CredentialPresentation::ApiKeyHeader))
                .credential_headers(),
            vec![("x-api-key", "sess-abc".to_owned())]
        );

        let oauth = endpoint_with(Credential::Oauth(OAuthCredential {
            access_token: Some(secret("access-1")),
            refresh_token: Some(secret("refresh-1")),
            expires_at: None,
        }));

        assert_eq!(
            Upstream::declared(&declaring(oauth, CredentialPresentation::ApiKeyHeader))
                .credential_headers(),
            vec![("x-api-key", "access-1".to_owned())],
            "still the access token and never the refresh one"
        );
    }

    #[test]
    fn a_presentation_this_satellite_has_never_heard_of_falls_back_to_inference() {
        // Enum values are additive within a proto major, so an older satellite
        // meets ones a newer caller set. Inferring beats refusing the turn.
        let mut endpoint = endpoint_with(Credential::ApiKey(secret("sk-ant-123")));
        endpoint.auth.as_mut().expect("auth was set").presentation = Some(9_999);

        assert_eq!(
            Upstream::declared(&endpoint).credential_headers(),
            vec![("x-api-key", "sk-ant-123".to_owned())]
        );
    }

    #[test]
    fn the_destination_decides_the_header_rather_than_the_key() {
        // Guessing a vendor from a key's prefix is the kind of rule that breaks
        // the first time a vendor changes one, and a host that merely contains
        // the string is not the host.
        assert!(api_key_is_a_bearer("https://api.openai.com/"));
        assert!(api_key_is_a_bearer("https://eu.api.openai.com/v1"));

        assert!(!api_key_is_a_bearer("https://api.anthropic.com"));
        assert!(!api_key_is_a_bearer("https://openai.com.example.invalid"));
        assert!(!api_key_is_a_bearer("not a url at all"));
    }

    #[test]
    fn a_declared_base_url_wins_over_the_providers_own() {
        let mut endpoint = endpoint_with(Credential::ApiKey(secret("k")));
        endpoint.base_url = Some("https://azure.example.com/anthropic/".to_owned());

        let upstream = Upstream::declared(&endpoint);

        assert_eq!(
            upstream.url_for("/v1/messages", None),
            "https://azure.example.com/anthropic/v1/messages",
            "a trailing slash on the base and a leading one on the path make one separator"
        );
    }

    #[test]
    fn a_query_string_survives_the_forward() {
        let upstream = Upstream::declared(&endpoint_with(Credential::ApiKey(secret("k"))));

        assert_eq!(
            upstream.url_for("v1/models", Some("limit=20")),
            "https://api.anthropic.com/v1/models?limit=20"
        );
    }

    #[test]
    fn a_thread_with_no_endpoint_still_resolves_somewhere() {
        // The chokepoint has to be universal. A thread that declared nothing
        // must still traverse the proxy, or its spending is unmeasured and its
        // credential is back in the agent's environment.
        let upstream = Upstream::ambient();

        assert_eq!(
            upstream.url_for("v1/messages", None),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn a_credential_with_no_plaintext_is_sent_as_none_rather_than_empty() {
        // `Secret` carries `display` on responses and `value` on requests. A
        // settings object that round-tripped through a response has the display
        // and no value, and presenting that would send the redacted rendering
        // as if it were the key.
        let redacted = Upstream::declared(&endpoint_with(Credential::ApiKey(Secret {
            value: None,
            display: Some("sk******23".to_owned()),
        })));

        assert!(redacted.credential_headers().is_empty());
    }
}
