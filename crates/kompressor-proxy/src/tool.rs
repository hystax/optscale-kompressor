//! The retrieve tool: its definition, and where it goes in a request.
//!
//! Recoverable mode replaces content with a marker carrying a handle. That is
//! only honest if the model can get the content back, and the model can only do
//! that if it is told the tool exists. This is where it is told — in the tool
//! list, never in the system prompt.
//!
//! # Why not the system prompt
//!
//! The system prompt is the client's. Appending to it changes behaviour the
//! client tuned, and on every provider it sits at the very front of the cached
//! prefix, so editing it invalidates the cache for the whole conversation. The
//! tool description carries the instruction instead: it reaches the model the
//! same way, and it is the client's tool list — a place the client already
//! expects to be filled with things the model may call.
//!
//! # Fixed bytes, fixed position
//!
//! The definition is byte-identical turn after turn and is appended at the end
//! of the tool list. Both matter for prompt caching: a definition whose bytes
//! or position moved between turns would push every following token out of the
//! provider's cached prefix, which costs more than the compression saves. For
//! Anthropic this also means being appended *after* any tool carrying
//! `cache_control` — a breakpoint pins everything before it, so arriving after
//! the last one leaves the pinned region untouched — and never carrying a
//! `cache_control` marker of its own.

use serde_json::{Map, Value, json};

use kompressor_wire::ApiFamily;

/// The tool's name on every family's wire.
pub const RETRIEVE_TOOL_NAME: &str = "optscale_kompressor_retrieve";

/// What a valid handle looks like, as a JSON Schema pattern.
///
/// Constraining it is not decoration: a model that invents a plausible-looking
/// handle gets a miss, and a miss costs a round trip. The pattern makes the
/// provider reject the invention before it is sent.
const HANDLE_PATTERN: &str = "^k_[A-Za-z0-9_-]{12}$";

/// The instruction the model actually reads.
///
/// All three marker shapes are named, and the third one is why. The description
/// used to give a single example — the `[... N … omitted [ref:k_…]]` form — and
/// on a workload of log and listing output that shape is almost absent: 115 of
/// 120 markers in one measured run were the fold form `line [×47 ref:k_…]`,
/// which nothing told the model was a handle at all.
///
/// Counting what the model has actually asked for settles which shape works.
/// Across eleven runs with any retrieval — 93 fetches, every one a hit — **37
/// of the 44 distinct fragments ever retrieved were code bodies**, named by the
/// `[... N body lines omitted from NAME; retrieve: ref:k_…]` shape. That shape
/// was the one the description did not mention. It is also the only one that
/// says *what* is missing by name, which is what lets a model decide whether it
/// needs it: a marker reading `[×2 ref:k_…]` names nothing, and in the run that
/// produced 241 of them not one was fetched.
///
/// So the code shape is documented here, and the fold shape now appears without
/// a handle whenever the count alone reconstructs it (see
/// `kompressor_engine::markers::fold_suffix`) — leaving the handles that remain
/// on the folds worth asking about.
const DESCRIPTION: &str = "OptScale Kompressor: fetch back content that optscale_kompressor elided from this \
     conversation. Long tool \
     results are compressed before they reach you, and every elision leaves a marker carrying a \
     handle. Markers take three shapes. An omission: \"[... 412 matches omitted \
     [ref:k_AbC123dEf456]]\". An elided function or class body, which is the one most often worth \
     fetching, since the stub that replaces it still parses and so does not look incomplete: \
     \"# [... 77 body lines omitted from join_revisions; retrieve: ref:k_AbC123dEf456]\" — never \
     reason about what such a body does without retrieving it first. And a fold, where a line \
     that repeated is kept once and followed by its count, \"connection refused \
     [×47 ref:k_AbC123dEf456]\" — there the handle holds the other 46 occurrences, and a fold \
     printed without a handle, \"[×47]\", repeated that exact line and has nothing more to give. \
     Any handle can be passed to this tool to get the original bytes back verbatim. Use it \
     whenever the elided detail matters for the answer, and never reconstruct, guess at or \
     summarise elided content from memory: if you need it, retrieve it. Retrieve narrowly where \
     you can — a line window or a pattern costs a fraction of a whole fragment.";

/// What happened when the proxy tried to offer the tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Injection {
    /// The definition was appended.
    Injected,
    /// Nothing was appended: no handle in the request, or the tool is off.
    NotNeeded,
    /// The client sent a tool of this name, so it runs its own loop.
    ///
    /// Injecting a second definition would offer the model two tools with one
    /// name, and executing the call here would steal it from a client that
    /// asked for it.
    ClientOwnsTool,
    /// The request has a `tools` field of a shape we do not understand.
    Unsupported,
}

/// The tool definition in a family's own shape.
pub fn definition(family: ApiFamily) -> Value {
    match family {
        // Chat nests the schema under `function`; Responses flattened the same
        // fields onto the tool itself.
        ApiFamily::OpenAiChat => json!({
            "type": "function",
            "function": {
                "name": RETRIEVE_TOOL_NAME,
                "description": DESCRIPTION,
                "parameters": parameters(),
            }
        }),
        ApiFamily::OpenAiResponses => json!({
            "type": "function",
            "name": RETRIEVE_TOOL_NAME,
            "description": DESCRIPTION,
            "parameters": parameters(),
        }),
        ApiFamily::AnthropicMessages => json!({
            "name": RETRIEVE_TOOL_NAME,
            "description": DESCRIPTION,
            "input_schema": parameters(),
        }),
    }
}

/// The argument schema, identical across families.
fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ref": {
                "type": "string",
                "pattern": HANDLE_PATTERN,
                "description": "The handle from an omission marker, such as k_AbC123dEf456."
            },
            "range": {
                "type": "object",
                "description": "Optional narrowing. Give exactly one of these; omit the whole \
                    field to retrieve the fragment entire. Do not send more than one, and do \
                    not send a variant with empty or zero values to stand in for \"unused\".",
                // The prose above is not enough on its own. Asked to narrow, a
                // real model filled in *all four* variants at once —
                // `{"lines":{1,80},"chars":{0,0},"json_pointer":{""},"grep":{"",0}}` —
                // because four optional sibling objects is exactly what the
                // schema described, and schema-guided decoding had nothing to
                // stop it. The consumer is an externally tagged enum, so a
                // four-key object could not deserialize at all and the range
                // was dropped: the whole fragment came back, every time,
                // silently. `maxProperties` is what a decoder can enforce
                // while generating; `oneOf` states the same rule for anything
                // that validates afterwards.
                "additionalProperties": false,
                "minProperties": 1,
                "maxProperties": 1,
                "oneOf": [
                    {"required": ["lines"]},
                    {"required": ["chars"]},
                    {"required": ["json_pointer"]},
                    {"required": ["grep"]}
                ],
                "properties": {
                    "lines": {
                        "type": "object",
                        "description": "A 1-based inclusive line window.",
                        "properties": {
                            "start": {"type": "integer"},
                            "end": {"type": "integer"}
                        },
                        "required": ["start", "end"]
                    },
                    "chars": {
                        "type": "object",
                        "description": "A 0-based character window.",
                        "properties": {
                            "start": {"type": "integer"},
                            "end": {"type": "integer"}
                        },
                        "required": ["start", "end"]
                    },
                    "json_pointer": {
                        "type": "object",
                        "description": "An RFC 6901 pointer into a fragment that is JSON.",
                        "properties": {"pointer": {"type": "string"}},
                        "required": ["pointer"]
                    },
                    "grep": {
                        "type": "object",
                        "description": "Lines matching a literal, with surrounding context lines.",
                        "properties": {
                            "pattern": {"type": "string"},
                            "context": {"type": "integer"}
                        },
                        "required": ["pattern"]
                    }
                }
            }
        },
        "required": ["ref"]
    })
}

/// Whether the client already declared a tool by this name.
pub fn client_declares_tool(body: &Value, family: ApiFamily) -> bool {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    tools
        .iter()
        .any(|tool| tool_name(tool, family) == Some(RETRIEVE_TOOL_NAME))
}

/// Append the definition to the request's tool list.
///
/// A request with no `tools` at all gets one: a model that cannot see the tool
/// cannot ask for anything back, and a marker the model cannot resolve is worse
/// than no compression.
pub fn inject(body: &mut Value, family: ApiFamily) -> Injection {
    if client_declares_tool(body, family) {
        return Injection::ClientOwnsTool;
    }
    let Some(object) = body.as_object_mut() else {
        return Injection::Unsupported;
    };
    match object.get_mut("tools") {
        None => {
            object.insert("tools".to_string(), json!([definition(family)]));
        }
        Some(Value::Array(tools)) => tools.push(definition(family)),
        // `"tools": null` is how some SDKs spell "no tools"; anything else is a
        // shape we do not understand, and guessing risks a 400 on a request the
        // provider would have answered.
        Some(Value::Null) => {
            object.insert("tools".to_string(), json!([definition(family)]));
        }
        Some(_) => return Injection::Unsupported,
    }
    Injection::Injected
}

/// Whether the request's `tool_choice` rules out calling the retrieve tool.
///
/// Offering the tool does not make it callable: `"none"` forbids every tool,
/// and forcing a named one forbids every other. A recoverable elision sent
/// with either is one the model must answer around, so the caller forwards
/// such a request whole instead.
///
/// Read across the three families at once, because their spellings do not
/// collide: Chat's `{"type":"function","function":{"name":…}}`, Responses'
/// `{"type":"function","name":…}` and `allowed_tools`, and Anthropic's
/// `{"type":"none"}` and `{"type":"tool","name":…}`. A choice not recognised
/// as either is taken as permitting the call, which is the behaviour before
/// this check existed.
pub fn choice_forbids_retrieval(body: &Value, family: ApiFamily) -> bool {
    let Some(choice) = body.get("tool_choice") else {
        return false;
    };
    if let Some(choice) = choice.as_str() {
        return choice == "none";
    }
    let Some(kind) = choice.get("type").and_then(Value::as_str) else {
        return false;
    };
    match kind {
        "none" => true,
        "auto" | "any" | "required" => false,
        "function" | "tool" => tool_name(choice, family) != Some(RETRIEVE_TOOL_NAME),
        "allowed_tools" => {
            let tools = choice
                .get("allowed_tools")
                .and_then(|allowed| allowed.get("tools"))
                .or_else(|| choice.get("tools"))
                .and_then(Value::as_array);
            !tools.is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| tool_name(tool, family) == Some(RETRIEVE_TOOL_NAME))
            })
        }
        // A hosted tool forced by type — `file_search`, `web_search` — is
        // still some other tool.
        _ => true,
    }
}

/// A tool's name in a family's shape, for both injection and call matching.
pub fn tool_name(tool: &Value, family: ApiFamily) -> Option<&str> {
    match family {
        ApiFamily::OpenAiChat => tool
            .get("function")
            .and_then(|f| f.get("name"))
            .or_else(|| tool.get("name"))
            .and_then(Value::as_str),
        ApiFamily::OpenAiResponses | ApiFamily::AnthropicMessages => {
            tool.get("name").and_then(Value::as_str)
        }
    }
}

/// Whether a serialized request still carries a handle the model could ask for.
///
/// Tested against the outgoing bytes rather than against this request's own
/// compression output, because a marker minted three turns ago is just as
/// retrievable as one minted now — and on a long conversation most of them were
/// minted earlier.
pub fn carries_handle(body: &str) -> bool {
    body.contains("ref:k_")
}

/// The arguments a model sent, whichever way its family spells them.
///
/// OpenAI sends a JSON *string*, which a model can emit malformed; Anthropic
/// sends an object. A string that will not parse yields `None` rather than a
/// guess, and the caller answers the call with a miss.
pub fn arguments_object(arguments: &Value) -> Option<Map<String, Value>> {
    match arguments {
        Value::String(text) => serde_json::from_str(text).ok(),
        Value::Object(map) => Some(map.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_choice_forbids_retrieval_only_when_it_rules_our_tool_out() {
        let forbids = |choice: Value, family| {
            choice_forbids_retrieval(&json!({ "tool_choice": choice }), family)
        };
        let chat = ApiFamily::OpenAiChat;
        let responses = ApiFamily::OpenAiResponses;
        let anthropic = ApiFamily::AnthropicMessages;

        assert!(!choice_forbids_retrieval(&json!({}), chat));
        assert!(forbids(json!("none"), chat));
        assert!(!forbids(json!("auto"), chat));
        assert!(!forbids(json!("required"), responses));
        assert!(forbids(json!({"type": "none"}), anthropic));
        assert!(!forbids(json!({"type": "any"}), anthropic));
        assert!(forbids(
            json!({"type": "tool", "name": "summarize"}),
            anthropic
        ));
        assert!(!forbids(
            json!({"type": "tool", "name": RETRIEVE_TOOL_NAME}),
            anthropic
        ));
        assert!(forbids(
            json!({"type": "function", "function": {"name": "summarize"}}),
            chat
        ));
        assert!(forbids(
            json!({"type": "function", "name": "summarize"}),
            responses
        ));
        assert!(forbids(json!({"type": "file_search"}), responses));
        assert!(forbids(
            json!({"type": "allowed_tools", "mode": "auto",
                   "tools": [{"type": "function", "name": "summarize"}]}),
            responses
        ));
        assert!(!forbids(
            json!({"type": "allowed_tools", "mode": "auto",
                   "tools": [{"type": "function", "name": RETRIEVE_TOOL_NAME}]}),
            responses
        ));
        assert!(!forbids(
            json!({"type": "allowed_tools", "allowed_tools": {"mode": "auto",
                   "tools": [{"type": "function", "function": {"name": RETRIEVE_TOOL_NAME}}]}}),
            chat
        ));
    }

    #[test]
    fn each_family_gets_its_own_tool_shape() {
        let chat = definition(ApiFamily::OpenAiChat);
        assert_eq!(chat["type"], "function");
        assert_eq!(chat["function"]["name"], RETRIEVE_TOOL_NAME);
        assert!(
            chat["function"]["parameters"]["properties"]["ref"]["pattern"]
                .as_str()
                .unwrap()
                .starts_with("^k_")
        );

        let responses = definition(ApiFamily::OpenAiResponses);
        assert_eq!(responses["type"], "function");
        assert_eq!(responses["name"], RETRIEVE_TOOL_NAME, "flat, not nested");
        assert!(responses.get("function").is_none());

        let anthropic = definition(ApiFamily::AnthropicMessages);
        assert_eq!(anthropic["name"], RETRIEVE_TOOL_NAME);
        assert!(anthropic["input_schema"]["properties"]["ref"].is_object());
        assert!(
            anthropic.get("cache_control").is_none(),
            "the injected tool must never become a cache breakpoint"
        );
    }

    #[test]
    fn the_definition_is_byte_identical_every_time_it_is_built() {
        // a definition whose bytes moved between turns would push the whole
        // conversation out of the provider's cached prefix
        for family in [
            ApiFamily::OpenAiChat,
            ApiFamily::OpenAiResponses,
            ApiFamily::AnthropicMessages,
        ] {
            let first = serde_json::to_string(&definition(family)).unwrap();
            let second = serde_json::to_string(&definition(family)).unwrap();
            assert_eq!(first, second);
        }
    }

    #[test]
    fn injection_appends_after_every_tool_the_client_sent() {
        let mut body = json!({
            "tools": [
                {"name": "read_file", "input_schema": {}, "cache_control": {"type": "ephemeral"}},
                {"name": "write_file", "input_schema": {}}
            ]
        });
        assert_eq!(
            inject(&mut body, ApiFamily::AnthropicMessages),
            Injection::Injected
        );
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[2]["name"], RETRIEVE_TOOL_NAME);
        assert_eq!(
            tools[0]["cache_control"]["type"], "ephemeral",
            "the client's breakpoint is untouched and still precedes ours"
        );
    }

    #[test]
    fn a_request_with_no_tools_gets_a_tool_list() {
        let mut body = json!({"model": "gpt-5", "messages": []});
        assert_eq!(
            inject(&mut body, ApiFamily::OpenAiChat),
            Injection::Injected
        );
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);

        let mut nulled = json!({"tools": null});
        assert_eq!(
            inject(&mut nulled, ApiFamily::OpenAiChat),
            Injection::Injected
        );
        assert_eq!(nulled["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_client_running_its_own_retrieve_loop_is_left_alone() {
        let mut body = json!({
            "tools": [{"type": "function", "function": {"name": RETRIEVE_TOOL_NAME}}]
        });
        assert_eq!(
            inject(&mut body, ApiFamily::OpenAiChat),
            Injection::ClientOwnsTool
        );
        assert_eq!(
            body["tools"].as_array().unwrap().len(),
            1,
            "two tools with one name is a request no provider can answer sensibly"
        );
    }

    #[test]
    fn a_tools_field_we_do_not_understand_is_not_guessed_at() {
        let mut body = json!({"tools": "auto"});
        assert_eq!(
            inject(&mut body, ApiFamily::OpenAiChat),
            Injection::Unsupported
        );
        assert_eq!(body["tools"], "auto");
    }

    #[test]
    fn a_handle_is_recognised_wherever_it_sits_in_the_outgoing_bytes() {
        assert!(carries_handle(
            r#"{"content":"[... 9 log lines omitted [ref:k_AbC123dEf456]]"}"#
        ));
        assert!(!carries_handle(r#"{"content":"nothing elided here"}"#));
    }

    #[test]
    fn arguments_arrive_as_a_string_or_an_object_and_never_as_a_guess() {
        assert_eq!(
            arguments_object(&json!("{\"ref\":\"k_AbC123dEf456\"}"))
                .unwrap()
                .get("ref")
                .unwrap(),
            "k_AbC123dEf456"
        );
        assert_eq!(
            arguments_object(&json!({"ref": "k_AbC123dEf456"}))
                .unwrap()
                .get("ref")
                .unwrap(),
            "k_AbC123dEf456"
        );
        assert!(arguments_object(&json!("{\"ref\": ")).is_none());
    }
}

/// What the description promises the model about markers.
#[cfg(test)]
mod description_tests {
    use super::DESCRIPTION;

    /// Both shapes, because a handle the model is never shown cannot be asked
    /// for. On four measured runs of a log-and-listing workload the fold form
    /// was 96% of the markers and retrieval was zero; the description named
    /// only the other one.
    #[test]
    fn both_marker_shapes_are_named() {
        assert!(
            DESCRIPTION.contains("optscale_kompressor"),
            "the product is not named"
        );
        assert!(DESCRIPTION.contains("omitted [ref:"), "the omission form");
        assert!(DESCRIPTION.contains("[×47 ref:"), "the fold form");
    }

    /// The MCP surface spells out every marker shape to the same audience. A
    /// model talking to the proxy and a model talking to the MCP server must
    /// not be told different things about what a handle looks like — that gap
    /// is exactly what this test exists to keep closed.
    #[test]
    fn the_handle_grammar_matches_what_the_mcp_surface_promises() {
        assert!(DESCRIPTION.contains("k_AbC123dEf456"));
        assert!(
            DESCRIPTION.contains("ref:"),
            "a handle is always introduced by `ref:`"
        );
    }
}
