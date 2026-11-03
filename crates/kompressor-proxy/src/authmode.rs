//! How the client in front of us pays, and what that permits us to change.
//!
//! The proxy has one header policy today and applies it to every caller. That
//! is wrong for one class of client and measurably so: a subscription CLI is
//! rate-limited by request count against a provider that fingerprints its own
//! agent, so every header we add or rewrite is drift the provider can see and
//! the caller did not ask for. An API key holder has no such exposure — they
//! pay per token and nothing about the request identifies a product.
//!
//! Measured against Claude Code 2.1.278, which sends:
//!
//! ```text
//! user-agent: claude-cli/2.1.278 (external, claude-desktop, agent-sdk/…)
//! accept-encoding: gzip, deflate, br, zstd
//! authorization: Bearer sk-ant-oat01-…
//! ```
//!
//! and to which the proxy currently adds an `x-request-id` the real client
//! never sends.
//!
//! The classification is advisory: it decides what we *refrain* from doing, it
//! never enables a transform. A misread here costs at most a lost correlation
//! id, which is why the default on anything ambiguous is the permissive one.

use http::HeaderMap;

/// How the caller in front of the proxy pays for tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthMode {
    /// A per-token API key. Nothing about the request identifies a product, so
    /// the proxy's ordinary header policy applies.
    Payg,
    /// An OAuth bearer or cloud-IAM credential that is not a known CLI. Treated
    /// as pay-as-you-go for header purposes; broken out because the two are
    /// different facts and a future policy may want to tell them apart.
    OAuth,
    /// A subscription-bound CLI or IDE. The provider bills by request and
    /// recognises its own client, so the request is relayed as close to
    /// byte-identical as the proxy can manage.
    Subscription,
}

impl AuthMode {
    /// Stable label for logs and stats rows.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::Payg => "payg",
            AuthMode::OAuth => "oauth",
            AuthMode::Subscription => "subscription",
        }
    }

    /// Whether the proxy must avoid adding headers of its own to the upstream
    /// request.
    pub fn is_subscription(self) -> bool {
        self == AuthMode::Subscription
    }
}

/// User-Agent fragments that identify a subscription-bound CLI.
///
/// Matched as a substring of the lowercased value rather than a prefix: some of
/// these embed a parent agent's UA ahead of their own, and a prefix test would
/// miss exactly the nested case that matters.
const SUBSCRIPTION_AGENTS: &[&str] = &[
    "claude-cli/",
    "claude-code/",
    "codex-cli/",
    // The interactive codex (`codex_cli_rs` until 0.162, `codex-tui` from
    // 0.164), and the one inside the VS Code extension.
    "codex_cli_rs/",
    "codex-tui/",
    "codex_vscode/",
    "codex_exec/",
    "cursor/",
    "claude-vscode/",
    "github-copilot/",
    "anthropic-cli/",
];

/// Classify a request by its headers.
///
/// Both signals are needed and neither alone is enough.
///
/// The token shape alone gets Claude Code backwards: it carries an
/// `sk-ant-oat…` OAuth token and is a subscription client, not a generic OAuth
/// caller. The User-Agent alone gets the other half backwards: the same
/// binaries run against a plain API key, and there the caller pays per token,
/// no subscription is at stake, and suppressing our correlation id costs
/// debuggability for nothing. So a request is `Subscription` only when a known
/// CLI is carrying something that is *not* a per-token key.
pub fn classify(headers: &HeaderMap) -> AuthMode {
    let agent = headers
        .get(http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let known_cli = SUBSCRIPTION_AGENTS
        .iter()
        .any(|needle| agent.contains(needle));
    // Codex on a ChatGPT plan names the account the request bills to, from
    // whichever of its front ends it runs — including those whose agent string
    // is not listed above. Nothing paying per token sends it.
    let chatgpt_account = headers.contains_key("chatgpt-account-id");
    if (known_cli || chatgpt_account) && !carries_api_key(headers) {
        return AuthMode::Subscription;
    }

    let bearer = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if let Some(value) = bearer {
        let token = value.strip_prefix("Bearer ").map(str::trim);
        match token {
            // `sk-ant-oat…` is Anthropic's OAuth token and shares the `sk-`
            // prefix with the API keys below, so it has to be tested first.
            Some(token) if token.starts_with("sk-ant-oat") => return AuthMode::OAuth,
            Some(token) if token.starts_with("sk-") => return AuthMode::Payg,
            // three dot-separated segments is a JWT, which no provider issues
            // as a per-token API key
            Some(token) if token.split('.').count() == 3 => return AuthMode::OAuth,
            Some(_) => return AuthMode::Payg,
            // a non-Bearer scheme is a signed cloud credential (SigV4 and
            // friends); never a per-token key
            None => return AuthMode::OAuth,
        }
    }

    if carries_api_key(headers) {
        return AuthMode::Payg;
    }
    AuthMode::Payg
}

/// Whether the caller presented a per-token API key rather than a subscription
/// or cloud credential.
///
/// `sk-ant-oat…` is excluded deliberately: it shares the `sk-` prefix with the
/// API keys but is Anthropic's OAuth token, which is exactly the subscription
/// case this has to keep.
fn carries_api_key(headers: &HeaderMap) -> bool {
    if headers.contains_key("x-api-key") || headers.contains_key("x-goog-api-key") {
        return true;
    }
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .is_some_and(|token| token.starts_with("sk-") && !token.starts_with("sk-ant-oat"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut out = HeaderMap::new();
        for (name, value) in pairs {
            out.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        out
    }

    #[test]
    fn a_subscription_cli_is_recognised_by_its_agent_not_its_token() {
        // The literal shape Claude Code 2.1.278 sends: an OAuth token carried
        // by a subscription client. Reading the token alone calls this OAuth,
        // which is the classification that loses the distinction.
        let claude = headers(&[
            (
                "user-agent",
                "claude-cli/2.1.278 (external, claude-desktop)",
            ),
            ("authorization", "Bearer sk-ant-oat01-EXAMPLE"),
        ]);
        assert_eq!(classify(&claude), AuthMode::Subscription);

        let codex = headers(&[("user-agent", "codex_exec/0.155.1 (Linux Mint)")]);
        assert_eq!(classify(&codex), AuthMode::Subscription);
    }

    /// The same binaries run against a plain API key. There the caller pays per
    /// token, there is no subscription to protect, and dropping our correlation
    /// id would cost debuggability for nothing.
    #[test]
    fn a_known_cli_holding_an_api_key_is_still_pay_as_you_go() {
        let codex_key = headers(&[
            ("user-agent", "codex_exec/0.155.1 (Linux Mint)"),
            ("authorization", "Bearer sk-proj-EXAMPLE"),
        ]);
        assert_eq!(classify(&codex_key), AuthMode::Payg);

        let claude_key = headers(&[
            ("user-agent", "claude-cli/2.1.278"),
            ("x-api-key", "sk-ant-api03-EXAMPLE"),
        ]);
        assert_eq!(classify(&claude_key), AuthMode::Payg);
    }

    /// The interactive codex on a ChatGPT plan, as its agent strings read in
    /// 0.162 and 0.164. Neither was listed, so without the account header
    /// these were OAuth, and went to api.openai.com.
    #[test]
    fn the_interactive_codex_on_a_chatgpt_plan_is_a_subscription() {
        for agent in [
            "codex_cli_rs/0.162.1 (Windows 10.0.26200; x86_64) WindowsTerminal",
            "codex-tui/0.164.0 (Windows 10.0.26200; x86_64) unknown (codex-tui; 0.164.0)",
        ] {
            let codex = headers(&[("user-agent", agent), ("authorization", "Bearer a.b.c")]);
            assert_eq!(classify(&codex), AuthMode::Subscription, "{agent}");
        }
    }

    /// The account header settles it whatever the agent string says, so a
    /// Codex front end not listed by name still reaches chatgpt.com. Codex
    /// 0.164 sends it with the plan's token; an API key in its place is not a
    /// subscription, account or not.
    #[test]
    fn a_chatgpt_account_marks_a_subscription_unless_a_key_is_sent() {
        let unlisted = headers(&[
            ("user-agent", "Codex Desktop/26.1021.31345"),
            ("authorization", "Bearer a.b.c"),
            ("chatgpt-account-id", "EXAMPLE"),
        ]);
        assert_eq!(classify(&unlisted), AuthMode::Subscription);

        let keyed = headers(&[
            ("authorization", "Bearer sk-proj-EXAMPLE"),
            ("chatgpt-account-id", "EXAMPLE"),
        ]);
        assert_eq!(classify(&keyed), AuthMode::Payg);
    }

    #[test]
    fn a_nested_agent_string_still_matches() {
        let nested = headers(&[("user-agent", "some-host/1.0 claude-cli/2.1.278")]);
        assert_eq!(classify(&nested), AuthMode::Subscription);
    }

    #[test]
    fn api_keys_are_pay_as_you_go() {
        assert_eq!(
            classify(&headers(&[(
                "authorization",
                "Bearer sk-ant-api03-EXAMPLE"
            )])),
            AuthMode::Payg
        );
        assert_eq!(
            classify(&headers(&[("authorization", "Bearer sk-proj-EXAMPLE")])),
            AuthMode::Payg
        );
        assert_eq!(
            classify(&headers(&[("x-api-key", "EXAMPLE")])),
            AuthMode::Payg
        );
    }

    #[test]
    fn oauth_without_a_known_agent_is_its_own_class() {
        assert_eq!(
            classify(&headers(&[(
                "authorization",
                "Bearer sk-ant-oat01-EXAMPLE"
            )])),
            AuthMode::OAuth
        );
        assert_eq!(
            classify(&headers(&[("authorization", "Bearer a.b.c")])),
            AuthMode::OAuth,
            "a JWT is not an API key"
        );
        assert_eq!(
            classify(&headers(&[(
                "authorization",
                "AWS4-HMAC-SHA256 Credential=EXAMPLE"
            )])),
            AuthMode::OAuth,
            "a signed cloud credential is not a per-token key"
        );
    }

    #[test]
    fn an_unidentifiable_caller_gets_the_permissive_default() {
        // Nothing is disabled by Payg, so the ambiguous case costs nothing.
        assert_eq!(classify(&HeaderMap::new()), AuthMode::Payg);
        let odd = headers(&[("user-agent", "curl/8.5.0")]);
        assert_eq!(classify(&odd), AuthMode::Payg);
    }

    #[test]
    fn a_non_utf8_agent_does_not_panic_or_misclassify() {
        let mut map = HeaderMap::new();
        map.insert(
            http::header::USER_AGENT,
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        map.insert("x-api-key", HeaderValue::from_static("EXAMPLE"));
        assert_eq!(classify(&map), AuthMode::Payg);
    }
}
