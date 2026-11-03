//! Resolving what this one request wants done to it.
//!
//! The proxy has a mode the engine does not: *off*. A deployment turning
//! compression off for one client, one model or the whole proxy needs the
//! traffic to keep flowing, so "off" has to be a value the proxy understands
//! rather than an engine mode nobody implements.

use std::str::FromStr;

use kompressor_engine::Mode;

/// What the proxy will do with a request's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyMode {
    /// Forward the text untouched: nothing is compressed and nothing is
    /// injected. The proxy still names the conversation and records the model
    /// and usage, so an off run is a control the compressed one can be scored
    /// against, and still asks for one-hour cache entries when
    /// `proxy.cache_ttl` says so.
    Off,
    /// Compress with this engine mode.
    On(Mode),
}

impl ProxyMode {
    /// Parse the `X-Kompressor-Mode` header value.
    ///
    /// Case- and space-insensitive because this header is typed by hand far
    /// more often than any other part of the wire.
    pub fn parse(value: &str) -> Option<ProxyMode> {
        let value = value.trim().to_ascii_lowercase();
        if value == "off" || value == "none" || value == "disabled" {
            return Some(ProxyMode::Off);
        }
        Mode::from_str(&value).ok().map(ProxyMode::On)
    }

    /// The spelling reported back on `X-Kompressor-Mode`.
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyMode::Off => "off",
            ProxyMode::On(mode) => mode.as_str(),
        }
    }

    /// The engine mode, or `None` when the request is not to be compressed.
    pub fn engine_mode(self) -> Option<Mode> {
        match self {
            ProxyMode::Off => None,
            ProxyMode::On(mode) => Some(mode),
        }
    }

    /// Whether this request skips the compression path entirely.
    pub fn is_off(self) -> bool {
        self == ProxyMode::Off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_names_a_mode_or_turns_the_proxy_off_for_one_request() {
        assert_eq!(ProxyMode::parse("off"), Some(ProxyMode::Off));
        assert_eq!(ProxyMode::parse(" OFF "), Some(ProxyMode::Off));
        assert_eq!(
            ProxyMode::parse("recoverable"),
            Some(ProxyMode::On(Mode::Recoverable))
        );
        assert_eq!(
            ProxyMode::parse("Aggressive"),
            Some(ProxyMode::On(Mode::Aggressive))
        );
    }

    #[test]
    fn an_unknown_mode_is_not_silently_read_as_off() {
        // reading a typo as "off" would turn a misconfigured client into a
        // deployment that quietly compresses nothing
        assert_eq!(ProxyMode::parse("recoverble"), None);
        assert_eq!(ProxyMode::parse(""), None);
    }
}
