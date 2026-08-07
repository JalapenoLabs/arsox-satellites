// Copyright © 2026 Jalapeno Labs

//! Where a model request goes, and what credential it carries.

use arsox_sdk::proto::settings::v1::ModelEndpoint;
use arsox_sdk::proto::settings::v1::llm_auth::Credential;

/// The provider's own base URL, used when a thread declares no endpoint.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Credentials the satellite falls back to, in the order they are consulted.
///
/// Read from the satellite's own environment at boot and never placed in an
/// agent's. `AUTH_TOKEN` is a bearer token and `API_KEY` is a key header, which
/// is why the two are not interchangeable.
const AMBIENT_BEARER: &str = "ANTHROPIC_AUTH_TOKEN";
const AMBIENT_API_KEY: &str = "ANTHROPIC_API_KEY";

/// How a credential is presented to the provider.
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

/// One resolved destination for a turn's model requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    base_url: String,
    presentation: Presentation,
}

impl Upstream {
    /// Resolves where a turn's requests should go.
    ///
    /// A thread that declares endpoints uses the first. Order is strict and
    /// documented as strict, so the first entry is the one to try, and failover
    /// to later entries is a separate concern from choosing a destination.
    ///
    /// A thread that declares none still goes through the proxy, using whatever
    /// credential the satellite itself holds. That is what keeps the chokepoint
    /// universal: an unconfigured thread must not be a thread whose spending is
    /// unmeasured and whose credential sits in the agent's environment.
    #[must_use]
    pub fn resolve(endpoints: &[ModelEndpoint]) -> Self {
        endpoints.first().map_or_else(Self::ambient, Self::declared)
    }

    fn declared(endpoint: &ModelEndpoint) -> Self {
        let presentation = match endpoint
            .auth
            .as_ref()
            .and_then(|auth| auth.credential.as_ref())
        {
            Some(Credential::ApiKey(secret)) => secret
                .value
                .clone()
                .map_or(Presentation::None, Presentation::ApiKey),
            Some(Credential::SubscriptionToken(secret)) => secret
                .value
                .clone()
                .map_or(Presentation::None, Presentation::Bearer),
            Some(Credential::Oauth(oauth)) => oauth
                .access_token
                .as_ref()
                .and_then(|secret| secret.value.clone())
                .map_or(Presentation::None, Presentation::Bearer),
            None => Presentation::None,
        };

        Self {
            base_url: endpoint
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
            presentation,
        }
    }

    fn ambient() -> Self {
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
    use arsox_sdk::proto::settings::v1::{LlmAuth, OAuthCredential};

    fn endpoint_with(credential: Credential) -> ModelEndpoint {
        ModelEndpoint {
            name: "primary".to_owned(),
            model: "claude-opus-5".to_owned(),
            base_url: None,
            auth: Some(LlmAuth {
                credential: Some(credential),
            }),
            retry: None,
        }
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
        let upstream = Upstream::resolve(&[]);

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
