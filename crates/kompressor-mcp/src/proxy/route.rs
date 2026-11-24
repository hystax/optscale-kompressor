//! Which server owns a tool name.
//!
//! Every fronted server's tools are exported under `<server>__<tool>`. Two
//! servers exporting `read_file` is ordinary, and silently shadowing one
//! presents as "the agent calls the wrong file reader sometimes" — a bug that
//! looks like model behaviour and is not.

/// What separates a server's name from the tool's own.
///
/// Two underscores rather than one: MCP tool names are conventionally
/// snake_case, so a single underscore would make `fs_read_file` ambiguous
/// between the `fs` server's `read_file` and an unprefixed `fs_read_file`.
pub const SEPARATOR: &str = "__";

/// The name a fronted tool is exported as.
pub fn qualify(server: &str, tool: &str) -> String {
    format!("{server}{SEPARATOR}{tool}")
}

/// Split an exported name back into server and tool.
///
/// `None` means the name carries no known prefix, which is how a call for one
/// of our own tools is recognised. Splitting on the *first* separator matters:
/// a tool legitimately named `read__file` behind server `fs` exports as
/// `fs__read__file` and must route to `fs` with `read__file` intact.
pub fn split<'a>(name: &'a str, servers: &[String]) -> Option<(&'a str, &'a str)> {
    for server in servers {
        let prefix = format!("{server}{SEPARATOR}");
        if let Some(tool) = name.strip_prefix(&prefix) {
            // Found by prefix rather than by splitting blindly, so an unknown
            // prefix falls through to our own tools instead of being routed
            // to a server that does not exist.
            let server_len = server.len();
            return Some((&name[..server_len], tool));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers() -> Vec<String> {
        vec!["fs".to_string(), "wiki".to_string()]
    }

    #[test]
    fn qualifies_and_splits_back() {
        let name = qualify("fs", "read_file");
        assert_eq!(name, "fs__read_file");
        assert_eq!(split(&name, &servers()), Some(("fs", "read_file")));
    }

    #[test]
    fn a_tool_whose_own_name_contains_the_separator_survives() {
        let name = qualify("fs", "read__file");
        assert_eq!(split(&name, &servers()), Some(("fs", "read__file")));
    }

    #[test]
    fn an_unknown_prefix_is_not_routed() {
        // Falls through to our own tools rather than naming a server that is
        // not configured.
        assert_eq!(split("other__read_file", &servers()), None);
        assert_eq!(split("optscale_kompressor_retrieve", &servers()), None);
    }

    #[test]
    fn two_servers_do_not_shadow_each_other() {
        let a = qualify("fs", "read_file");
        let b = qualify("wiki", "read_file");
        assert_ne!(a, b);
        assert_eq!(split(&a, &servers()).unwrap().0, "fs");
        assert_eq!(split(&b, &servers()).unwrap().0, "wiki");
    }
}
