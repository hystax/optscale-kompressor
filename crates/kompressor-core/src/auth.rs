//! Turning a credential into a tenant.
//!
//! The tenant is the scoping unit for everything durable: retained fragments key
//! on it, and so does every statistic. Resolving it is therefore the first thing
//! a request does, and the only thing that can decide it — a caller cannot name
//! its own tenant unless it holds the shared secret.

use kompressor_engine::Mode;
use kompressor_store::types::Tenant;

use crate::config::{AuthConfig, AuthMode};
use crate::error::AuthError;

/// What a resolved credential grants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub tenant: Tenant,
    /// A per-tenant override of the compression mode, if configured.
    pub mode: Option<Mode>,
    /// Set when the caller authenticated with the shared secret rather than an
    /// API key, and may therefore name a tenant explicitly.
    pub trusted: bool,
}

impl Identity {
    pub fn anonymous() -> Identity {
        Identity {
            tenant: Tenant::anonymous(),
            mode: None,
            trusted: false,
        }
    }
}

/// The credentials a request presented.
#[derive(Clone, Debug, Default)]
pub struct Credentials<'a> {
    /// `Authorization: Bearer <key>`, with the scheme already stripped.
    pub bearer: Option<&'a str>,
    /// `X-Api-Key`.
    ///
    /// Read as well as the bearer form: a client authenticating the way the
    /// Anthropic SDK does sends `x-api-key` and nothing else, and a credential
    /// the proxy does not recognise means traffic silently never compressed.
    pub api_key: Option<&'a str>,
    /// `X-Kompressor-Key`, the header that exists to be *ours*.
    ///
    /// Separate from [`Self::api_key`] because the whole point of a dedicated
    /// header is to authenticate to this proxy while a different credential
    /// goes to the provider. Folded in with the others it lost: the resolver
    /// used to try one candidate — the bearer, when present — so a caller
    /// sending a provider `Authorization` *and* a valid `X-Kompressor-Key`
    /// resolved to `Unknown`, fell open to the anonymous tenant, and shared a
    /// fragment namespace with every other caller that did the same. Handles
    /// are content digests, not access tokens, so that is a cross-tenant read.
    pub kompressor_key: Option<&'a str>,
    /// `X-Kompressor-Secret`, the trusted internal caller's credential.
    pub secret: Option<&'a str>,
    /// `X-Kompressor-Tenant`, honoured only alongside a valid secret.
    pub requested_tenant: Option<&'a str>,
}

impl<'a> Credentials<'a> {
    /// Every credential this request presented, ours first.
    ///
    /// All of them are tried, because a request may carry one credential for
    /// this proxy and another for the provider, and only the caller knows
    /// which is which. Order decides only which wins when more than one is a
    /// configured key, and there the dedicated header is the deliberate
    /// choice.
    fn candidates(&self) -> impl Iterator<Item = &'a str> + '_ {
        [self.kompressor_key, self.api_key, self.bearer]
            .into_iter()
            .flatten()
    }

    fn presented_any(&self) -> bool {
        self.candidates().next().is_some()
    }
}

pub struct Authenticator {
    config: AuthConfig,
}

impl Authenticator {
    pub fn new(config: AuthConfig) -> Authenticator {
        Authenticator { config }
    }

    pub fn mode(&self) -> AuthMode {
        self.config.mode
    }

    /// Resolve a request's identity, or say why it cannot be resolved.
    pub fn resolve(&self, credentials: &Credentials<'_>) -> Result<Identity, AuthError> {
        // The shared secret is checked first and independently of the auth mode:
        // it is how a trusted component in the same deployment attributes work to
        // a tenant it is acting for.
        if !self.config.secret.is_empty()
            && let Some(secret) = credentials.secret
        {
            if constant_time_eq(secret, &self.config.secret) {
                let tenant = credentials
                    .requested_tenant
                    .map(Tenant::new)
                    .unwrap_or_else(Tenant::anonymous);
                return Ok(Identity {
                    tenant,
                    mode: None,
                    trusted: true,
                });
            }
            return Err(AuthError::Unknown);
        }

        if self.config.mode == AuthMode::None {
            return Ok(Identity::anonymous());
        }

        if !credentials.presented_any() {
            return Err(AuthError::Missing);
        }
        // Candidates in order until one matches a key: a provider credential
        // that is not one of ours must not mask a dedicated key that is.
        //
        // Not an exhaustive scan, and the distinction matters for what this
        // does and does not promise. `find_map` and `find` both short-circuit,
        // so the number of `constant_time_eq` calls depends on where the match
        // is. Each comparison is constant-time, so a key's *contents* do not
        // leak; the count does, exactly as it did when one candidate was tried
        // against the same list. This makes that no worse — it is not a
        // constant-time resolver, and nothing here should be read as claiming
        // one.
        let matched = credentials.candidates().find_map(|presented| {
            self.config
                .keys
                .iter()
                .find(|k| constant_time_eq(presented, &k.key))
        });
        match matched {
            None => Err(AuthError::Unknown),
            Some(key) if !key.enabled => Err(AuthError::Disabled),
            Some(key) => Ok(Identity {
                tenant: Tenant::new(key.tenant.clone()),
                mode: key.mode,
                trusted: false,
            }),
        }
    }
}

/// Compare without leaking where two credentials first differ.
///
/// The comparison is over bytes rather than characters, and always visits the
/// whole of the longer input, so neither the position of a mismatch nor the
/// length of the configured key is observable from timing.
fn constant_time_eq(left: &str, right: &str) -> bool {
    let (a, b) = (left.as_bytes(), right.as_bytes());
    // Not `(a.len() ^ b.len()) as u8`: that keeps only the low byte, so two
    // lengths 256 apart read as equal and the zero padding below then matches
    // a key followed by NULs.
    let mut diff = u8::from(a.len() != b.len());
    let len = a.len().max(b.len());
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiKey;

    fn keyed() -> Authenticator {
        Authenticator::new(AuthConfig {
            mode: AuthMode::ApiKeys,
            secret: "cluster-secret".into(),
            keys: vec![
                ApiKey {
                    key: "sk-alpha".into(),
                    tenant: "team-a".into(),
                    enabled: true,
                    mode: Some(Mode::Aggressive),
                },
                ApiKey {
                    key: "sk-retired".into(),
                    tenant: "team-b".into(),
                    enabled: false,
                    mode: None,
                },
            ],
        })
    }

    #[test]
    fn without_auth_every_caller_is_one_anonymous_tenant() {
        let auth = Authenticator::new(AuthConfig::default());
        let identity = auth.resolve(&Credentials::default()).unwrap();
        assert_eq!(identity.tenant, Tenant::anonymous());
        assert!(!identity.trusted);
    }

    #[test]
    fn a_key_resolves_to_its_tenant_and_mode() {
        let identity = keyed()
            .resolve(&Credentials {
                bearer: Some("sk-alpha"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(identity.tenant.as_str(), "team-a");
        assert_eq!(identity.mode, Some(Mode::Aggressive));
    }

    #[test]
    fn the_anthropic_style_header_is_accepted_too() {
        // reading only the bearer form meant a client authenticating the way the
        // Anthropic SDK does presented nothing we recognised, and its traffic was
        // silently never compressed
        let identity = keyed()
            .resolve(&Credentials {
                api_key: Some("sk-alpha"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(identity.tenant.as_str(), "team-a");
    }

    /// A provider credential must not mask our own.
    ///
    /// This is what `X-Kompressor-Key` is *for*: authenticate to this proxy
    /// while a different credential goes to the provider. The resolver used to
    /// try one candidate, the bearer, so this combination resolved to
    /// `Unknown` and the proxy fell open to the anonymous tenant. Two
    /// authenticated tenants doing that share one fragment namespace, and
    /// handles are content digests rather than access tokens — so a handle
    /// from one tenant retrieved under another returned its content.
    #[test]
    fn a_provider_bearer_does_not_mask_our_own_key() {
        let identity = keyed()
            .resolve(&Credentials {
                bearer: Some("sk-the-providers-own-key"),
                kompressor_key: Some("sk-alpha"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(identity.tenant.as_str(), "team-a");
    }

    /// And the same the other way round, for a client that sends its provider
    /// credential in the Anthropic-style header.
    #[test]
    fn a_provider_api_key_header_does_not_mask_our_own_key() {
        let identity = keyed()
            .resolve(&Credentials {
                api_key: Some("sk-the-providers-own-key"),
                kompressor_key: Some("sk-alpha"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(identity.tenant.as_str(), "team-a");
    }

    /// Two tenants, each presenting its own key alongside its own provider
    /// credential, must not collapse into one namespace.
    #[test]
    fn two_tenants_behind_provider_credentials_stay_apart() {
        // Its own fixture: `keyed()`'s second key is deliberately disabled, and
        // this needs two live tenants.
        let auth = Authenticator::new(AuthConfig {
            mode: AuthMode::ApiKeys,
            secret: String::new(),
            keys: vec![
                ApiKey {
                    key: "sk-alpha".into(),
                    tenant: "team-a".into(),
                    enabled: true,
                    mode: None,
                },
                ApiKey {
                    key: "sk-beta".into(),
                    tenant: "team-b".into(),
                    enabled: true,
                    mode: None,
                },
            ],
        });
        let a = auth
            .resolve(&Credentials {
                bearer: Some("provider-a"),
                kompressor_key: Some("sk-alpha"),
                ..Default::default()
            })
            .unwrap();
        let b = auth
            .resolve(&Credentials {
                bearer: Some("provider-b"),
                kompressor_key: Some("sk-beta"),
                ..Default::default()
            })
            .unwrap();
        assert_ne!(a.tenant.as_str(), b.tenant.as_str());
        assert_ne!(a.tenant.as_str(), Tenant::anonymous().as_str());
    }

    #[test]
    fn an_unknown_or_disabled_key_is_refused() {
        let auth = keyed();
        assert_eq!(
            auth.resolve(&Credentials {
                bearer: Some("sk-nope"),
                ..Default::default()
            }),
            Err(AuthError::Unknown)
        );
        assert_eq!(
            auth.resolve(&Credentials {
                bearer: Some("sk-retired"),
                ..Default::default()
            }),
            Err(AuthError::Disabled)
        );
        assert_eq!(
            auth.resolve(&Credentials::default()),
            Err(AuthError::Missing)
        );
    }

    #[test]
    fn only_the_shared_secret_may_name_a_tenant() {
        let auth = keyed();
        // a key holder cannot reach another tenant by asking
        let identity = auth
            .resolve(&Credentials {
                bearer: Some("sk-alpha"),
                requested_tenant: Some("team-b"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(identity.tenant.as_str(), "team-a");

        let trusted = auth
            .resolve(&Credentials {
                secret: Some("cluster-secret"),
                requested_tenant: Some("team-b"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(trusted.tenant.as_str(), "team-b");
        assert!(trusted.trusted);
    }

    #[test]
    fn a_wrong_secret_is_refused_rather_than_falling_back_to_a_key() {
        assert_eq!(
            keyed().resolve(&Credentials {
                secret: Some("guessed"),
                bearer: Some("sk-alpha"),
                ..Default::default()
            }),
            Err(AuthError::Unknown)
        );
    }

    #[test]
    fn credential_comparison_does_not_short_circuit() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
        // Lengths a multiple of 256 apart, the longer padded with NULs: the
        // length difference used to be truncated to its low byte and vanish.
        for extra in [256, 512, 65_536] {
            let padded = format!("k{}", "\0".repeat(extra));
            assert!(!constant_time_eq("k", &padded), "{extra} NULs");
            assert!(!constant_time_eq(&padded, "k"), "{extra} NULs, reversed");
        }
        assert!(!constant_time_eq("", &"\0".repeat(256)));
    }
}
