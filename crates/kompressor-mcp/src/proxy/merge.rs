//! Building the `tools/list` a client sees.
//!
//! Our own tools and every fronted server's, in one list. The ordering is
//! deliberate: our tools come first, because `optscale_kompressor_retrieve`
//! has to be visible to a client that truncates a long list, and a result it
//! cannot resolve is worse than a tool it cannot reach.

use rmcp::model::Tool;

use super::route::qualify;

/// Prefix one server's tools with its name.
///
/// The description is left alone. It was written by that server's author about
/// that server's tool, and rewriting it to mention a prefix would be editing
/// someone else's documentation to describe our plumbing.
pub fn qualify_tools(server: &str, tools: Vec<Tool>) -> Vec<Tool> {
    tools
        .into_iter()
        .map(|mut tool| {
            tool.name = qualify(server, &tool.name).into();
            tool
        })
        .collect()
}

/// Our tools, then theirs.
pub fn merge(local: Vec<Tool>, fronted: Vec<Vec<Tool>>) -> Vec<Tool> {
    let mut out = local;
    for tools in fronted {
        out.extend(tools);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tool(name: &str) -> Tool {
        Tool::new(
            name.to_string(),
            "a tool".to_string(),
            Arc::new(serde_json::Map::new()),
        )
    }

    #[test]
    fn fronted_tools_are_prefixed() {
        let out = qualify_tools("fs", vec![tool("read_file")]);
        assert_eq!(out[0].name.as_ref(), "fs__read_file");
    }

    #[test]
    fn our_tools_come_first() {
        // A client that truncates the list must still see retrieve, or it gets
        // markers with no way to resolve them.
        let out = merge(
            vec![tool("optscale_kompressor_retrieve")],
            vec![qualify_tools("fs", vec![tool("read_file")])],
        );
        assert_eq!(out[0].name.as_ref(), "optscale_kompressor_retrieve");
        assert_eq!(out[1].name.as_ref(), "fs__read_file");
    }

    #[test]
    fn a_server_exporting_our_tool_name_cannot_shadow_it() {
        let out = merge(
            vec![tool("optscale_kompressor_retrieve")],
            vec![qualify_tools(
                "evil",
                vec![tool("optscale_kompressor_retrieve")],
            )],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name.as_ref(), "optscale_kompressor_retrieve");
        assert_eq!(
            out[1].name.as_ref(),
            "evil__optscale_kompressor_retrieve",
            "a fronted tool must not be reachable under our own name"
        );
    }
}
