//! Which hosts this listener terminates TLS for.
//!
//! Exact, lowercased hostnames. The configuration documents why there is no
//! pattern syntax here; the short version is that a CONNECT proxy which
//! intercepts one host more than it meant to looks exactly like one that does
//! not, and equality has no way to drift.

/// The hosts whose TLS is terminated. Anything absent is tunnelled.
#[derive(Clone, Debug, Default)]
pub struct AllowList {
    hosts: Vec<String>,
}

impl AllowList {
    /// Lowercases once, here, so matching never has to.
    pub fn new<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        AllowList {
            hosts: hosts
                .into_iter()
                .map(|h| h.as_ref().trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }

    /// Whether `host` — a bare hostname, no port — is intercepted.
    ///
    /// A hostname is compared whole. `api.anthropic.com.evil.test` shares a
    /// prefix with an allowed host and is not one, which a `starts_with` would
    /// get wrong in the dangerous direction.
    pub fn intercepts(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        self.hosts.contains(&host)
    }

    /// Nothing listed means nothing intercepted, which is the safe reading of
    /// an empty list: a listener that tunnels everything is useless, not
    /// dangerous.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exactly_and_ignores_case() {
        let list = AllowList::new(["api.anthropic.com"]);
        assert!(list.intercepts("api.anthropic.com"));
        assert!(list.intercepts("API.Anthropic.COM"));
        assert!(!list.intercepts("anthropic.com"));
    }

    #[test]
    fn a_near_miss_is_tunnelled_not_intercepted() {
        // The failure that a prefix or a loose pattern would produce, and the
        // reason this is equality.
        let list = AllowList::new(["api.anthropic.com"]);
        assert!(!list.intercepts("api.anthropic.com.evil.test"));
        assert!(!list.intercepts("notapi.anthropic.com"));
        assert!(!list.intercepts("api.anthropic.co"));
    }

    #[test]
    fn a_trailing_root_label_still_matches() {
        // `host.` and `host` are the same name; a client is free to send either.
        assert!(AllowList::new(["api.anthropic.com"]).intercepts("api.anthropic.com."));
    }

    #[test]
    fn empty_entries_are_dropped_and_an_empty_list_intercepts_nothing() {
        let list = AllowList::new(["", "  "]);
        assert!(list.is_empty());
        assert!(!list.intercepts(""));
    }
}
