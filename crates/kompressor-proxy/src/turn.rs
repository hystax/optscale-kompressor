//! Reading a completed assistant turn, and writing the next request from it.
//!
//! Three families, three spellings of the same two facts: what the model asked
//! for, and how to tell it what it got. Everything family-specific about the
//! non-streaming retrieve loop is in this module, so the loop itself reads the
//! same for all three.
//!
//! Mixed turns return the client's tool calls, with the proxy's private calls
//! removed, matching the streamed handover policy.
//!
//! What that costs, stated so it is a decision rather than a surprise: the
//! model's retrieve call is dropped **unanswered**. It asked for an elided
//! fragment and gets no result and no refusal. The alternative is worse —
//! answering our call means putting a tool result into a conversation the
//! client is also being handed, for a call the client never saw, and the
//! provider rejects the next request for a result it has no call for. The
//! marker and its handle are still in the context, so the model can ask again
//! on a later turn; a model that *always* pairs a retrieve with another tool
//! call never gets one, which is a capability limit rather than a loop, since
//! the client's own call still makes progress each turn.

use serde_json::{Map, Value, json};

use kompressor_wire::ApiFamily;

use crate::tool::RETRIEVE_TOOL_NAME;

/// One tool call the model made.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelToolCall {
    /// The id a tool result must quote: `tool_call_id`, `call_id` or
    /// `tool_use_id` depending on the family.
    pub id: String,
    /// The tool's name as the model called it.
    pub name: String,
    /// The arguments, as a JSON string (OpenAI) or an object (Anthropic).
    pub arguments: Value,
}

/// What the model did with its turn.
#[derive(Clone, Debug, PartialEq)]
pub enum Turn {
    /// It answered. Nothing for the proxy to do.
    Answered,
    /// It asked only for elided content.
    Retrieve(Vec<ModelToolCall>),
    /// It called something else — possibly alongside ours.
    OtherTool,
}

/// Whether a Responses output item of this `type` is a call the *client* has
/// to answer, other than a plain `function_call`.
///
/// The Responses API has more call shapes than `function_call`: Codex's
/// `apply_patch` and `exec` arrive as `custom_tool_call`, and there are
/// `local_shell_call`, `computer_call` and an MCP approval request besides.
/// Each one is the client's turn to finish. The calls the provider runs itself
/// (web and file search, the code interpreter, image generation, a remote MCP
/// call) come back already answered and are nobody's turn.
pub(crate) fn is_client_side_call(kind: &str) -> bool {
    const PROVIDER_RUN: [&str; 5] = [
        "web_search_call",
        "file_search_call",
        "code_interpreter_call",
        "image_generation_call",
        "mcp_call",
    ];
    kind == "mcp_approval_request"
        || (kind.ends_with("_call") && kind != "function_call" && !PROVIDER_RUN.contains(&kind))
}

/// Whether a Responses turn carries a client-side call that is not a
/// `function_call`.
fn has_client_side_call(family: ApiFamily, response: &Value) -> bool {
    family == ApiFamily::OpenAiResponses
        && response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type")
                        .and_then(Value::as_str)
                        .is_some_and(is_client_side_call)
                })
            })
}

/// Classify a complete (non-streamed) provider response.
pub fn classify(family: ApiFamily, response: &Value) -> Turn {
    let calls = tool_calls(family, response);
    if calls.is_empty() {
        return Turn::Answered;
    }
    if calls.iter().any(|call| call.name != RETRIEVE_TOOL_NAME) {
        return Turn::OtherTool;
    }
    // Ours next to a call only the client can answer is the client's turn. Seen
    // as retrieve-only, the round was run and the `custom_tool_call` replayed
    // upstream with no output to match it, while the client never saw its call.
    if has_client_side_call(family, response) {
        return Turn::OtherTool;
    }
    // `n > 1` asked for several independent completions; a retrieve round can
    // only continue one of them, and picking is not ours to do. Treated as a
    // foreign turn so the private calls are stripped and the rest is handed
    // over whole — the one thing that must not happen is the client seeing a
    // `optscale_kompressor_retrieve` call it never offered and cannot answer.
    if choice_count(family, response) > 1 {
        return Turn::OtherTool;
    }
    Turn::Retrieve(calls)
}

/// How many independent completions the response carries. Only the Chat family
/// has the concept; the others answer once.
fn choice_count(family: ApiFamily, response: &Value) -> usize {
    match family {
        ApiFamily::OpenAiChat => response
            .get("choices")
            .and_then(Value::as_array)
            .map_or(1, Vec::len),
        _ => 1,
    }
}

/// Remove the private recovery tool before handing a mixed turn to the client.
///
/// The `finish_reason` and `stop_reason` rewrites below are defensive and
/// currently unreachable: the sole caller is the `Turn::OtherTool` arm, and
/// [`classify`] answers `OtherTool` only when some call is *not* ours — so a
/// foreign call always survives the strip and the terminal field still reads
/// true. They are kept for the day something else calls this with a response
/// holding only our calls. The Responses family has no equivalent rewrite
/// because it has no equivalent field, not because one was forgotten.
pub(crate) fn strip_retrieve_calls(family: ApiFamily, response: &mut Value) -> bool {
    let mut changed = false;
    let mut strip = |items: &mut Vec<Value>, field: &str, kind: Option<&str>| {
        let before = items.len();
        items.retain(|item| {
            let candidate = if field == "function" {
                &item["function"]
            } else {
                item
            };
            !(candidate["name"] == RETRIEVE_TOOL_NAME
                && kind.is_none_or(|kind| item["type"] == kind))
        });
        changed |= items.len() != before;
    };
    match family {
        ApiFamily::OpenAiChat => {
            if let Some(choices) = response["choices"].as_array_mut() {
                for choice in choices {
                    if let Some(calls) = choice["message"]["tool_calls"].as_array_mut() {
                        strip(calls, "function", None);
                        if calls.is_empty() {
                            choice["finish_reason"] = json!("stop");
                        }
                    }
                }
            }
        }
        ApiFamily::OpenAiResponses => {
            if let Some(items) = response["output"].as_array_mut() {
                strip(items, "name", Some("function_call"));
            }
        }
        ApiFamily::AnthropicMessages => {
            if let Some(items) = response["content"].as_array_mut() {
                strip(items, "name", Some("tool_use"));
                if !items.iter().any(|item| item["type"] == "tool_use") {
                    response["stop_reason"] = json!("end_turn");
                }
            }
        }
        // No retrieve tool is offered on these families.
        ApiFamily::GeminiGenerateContent | ApiFamily::BedrockConverse => {}
    }
    changed
}

/// Every tool call in the response, in wire order.
pub fn tool_calls(family: ApiFamily, response: &Value) -> Vec<ModelToolCall> {
    match family {
        ApiFamily::OpenAiChat => response
            .get("choices")
            .and_then(Value::as_array)
            .map(|choices| {
                choices
                    .iter()
                    .filter_map(|choice| {
                        choice
                            .get("message")
                            .and_then(|message| message.get("tool_calls"))
                            .and_then(Value::as_array)
                    })
                    .flatten()
                    .filter_map(|call| {
                        let function = call.get("function")?;
                        Some(ModelToolCall {
                            id: call.get("id").and_then(Value::as_str)?.to_string(),
                            name: function.get("name").and_then(Value::as_str)?.to_string(),
                            arguments: function.get("arguments").cloned().unwrap_or(json!("{}")),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ApiFamily::OpenAiResponses => response
            .get("output")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        item.get("type").and_then(Value::as_str) == Some("function_call")
                    })
                    .filter_map(|item| {
                        Some(ModelToolCall {
                            // the call_id is what a function_call_output
                            // quotes; the item id addresses the item itself
                            id: item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(Value::as_str)?
                                .to_string(),
                            name: item.get("name").and_then(Value::as_str)?.to_string(),
                            arguments: item.get("arguments").cloned().unwrap_or(json!("{}")),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ApiFamily::AnthropicMessages => response
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                    .filter_map(|block| {
                        Some(ModelToolCall {
                            id: block.get("id").and_then(Value::as_str)?.to_string(),
                            name: block.get("name").and_then(Value::as_str)?.to_string(),
                            arguments: block.get("input").cloned().unwrap_or(json!({})),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        // Not read: the retrieve loop does not run on these families, so a
        // call in their response is always the client's.
        ApiFamily::GeminiGenerateContent | ApiFamily::BedrockConverse => Vec::new(),
    }
}

/// Append the model's turn and our answers to it, producing the next request.
///
/// The base prompt is left exactly as it was sent, which is the point: the
/// provider's cached prefix still covers everything up to the new tail, so a
/// retrieve round is billed as a short continuation rather than as a second
/// full prompt.
pub fn append_round(
    family: ApiFamily,
    request: &mut Value,
    response: &Value,
    results: &[(ModelToolCall, String)],
) -> bool {
    match family {
        ApiFamily::OpenAiChat => {
            let Some(message) = response
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("message"))
            else {
                return false;
            };
            let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
                return false;
            };
            messages.push(message.clone());
            for (call, result) in results {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": result,
                }));
            }
            true
        }
        ApiFamily::OpenAiResponses => {
            let Some(output) = response.get("output").and_then(Value::as_array) else {
                return false;
            };
            let Some(input) = request.get_mut("input") else {
                return false;
            };
            // `input` may have been sent as a bare string, which is shorthand
            // for one user message; it has to become a list before a turn can
            // be appended to it.
            if let Value::String(text) = input {
                *input = json!([{"role": "user", "content": text}]);
            }
            let Some(items) = input.as_array_mut() else {
                return false;
            };
            // every output item, not only the calls: a reasoning item dropped
            // here is reasoning the model has to redo on the next round
            items.extend(output.iter().cloned());
            for (call, result) in results {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": call.id,
                    "output": result,
                }));
            }
            true
        }
        ApiFamily::AnthropicMessages => {
            let Some(content) = response.get("content") else {
                return false;
            };
            let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
                return false;
            };
            messages.push(json!({"role": "assistant", "content": content}));
            // one user message holding every result: Anthropic requires the
            // tool_result blocks for a turn to arrive together, in the turn
            // immediately after the calls
            let blocks: Vec<Value> = results
                .iter()
                .map(|(call, result)| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": call.id,
                        "content": result,
                    })
                })
                .collect();
            messages.push(json!({"role": "user", "content": blocks}));
            true
        }
        ApiFamily::GeminiGenerateContent | ApiFamily::BedrockConverse => false,
    }
}

/// The usage object of a complete response.
pub fn usage_of(response: &Value) -> Option<&Value> {
    response
        .get("usage")
        .or_else(|| response.get("usageMetadata"))
        .filter(|usage| usage.is_object())
}

/// Add every number in `add` into `acc`, recursively.
///
/// Recursive because the interesting numbers are nested one level down on two
/// of the three families (`prompt_tokens_details.cached_tokens`), and because
/// summing only the top level would report a cache hit rate that quietly
/// ignored every round but the last.
pub fn add_usage(acc: &mut Value, add: &Value) {
    match (acc, add) {
        (Value::Object(acc), Value::Object(add)) => {
            for (key, value) in add {
                match acc.get_mut(key) {
                    Some(existing) => add_usage(existing, value),
                    None => {
                        acc.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (acc @ Value::Number(_), Value::Number(add)) => {
            let sum = acc.as_f64().unwrap_or(0.0) + add.as_f64().unwrap_or(0.0);
            *acc = number(sum);
        }
        // a non-numeric field (a model name, a service tier) is not summable;
        // the later round's value stands, which is the one that produced the
        // response the client is about to read
        (acc, add) => *acc = add.clone(),
    }
}

/// Fold one frame's usage into the running total **for a single round**.
///
/// The counters in a round are levels, not increments: Anthropic reports the
/// same `input_tokens`, `cache_read_input_tokens` and
/// `cache_creation_input_tokens` in `message_start` and again in
/// `message_delta`, both describing the one call. Summing them — which is what
/// this round used to do, because [`add_usage`] is the right answer *across*
/// rounds and was reached for here too — double-counts the entire input side.
///
/// Measured on a Harbor run before this existed: the proxy reported 1,978,478
/// cache-read tokens where the client reading the same responses saw 989,239,
/// a ratio of exactly 2.0000. Output tokens were untouched, because
/// `output_tokens` is a placeholder in `message_start` and only meaningful in
/// `message_delta` — which is precisely the asymmetry that makes the maximum
/// the right fold: it takes the real figure from whichever frame carries it.
///
/// The maximum rather than "the last frame wins", because the older wire shape
/// puts input counters in `message_start` alone and omits them from
/// `message_delta`; letting the later frame stand would erase them. And rather
/// than naming the input fields explicitly, because a counter this proxy has
/// never heard of should still survive a provider adding it.
///
/// Every `message_delta` fixture in this crate carried `output_tokens` alone —
/// the old shape — so the suite agreed with the bug for as long as it existed.
pub fn max_usage(acc: &mut Value, add: &Value) {
    match (acc, add) {
        (Value::Object(acc), Value::Object(add)) => {
            for (key, value) in add {
                match acc.get_mut(key) {
                    Some(existing) => max_usage(existing, value),
                    None => {
                        acc.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (acc @ Value::Number(_), Value::Number(add)) => {
            let highest = acc.as_f64().unwrap_or(0.0).max(add.as_f64().unwrap_or(0.0));
            *acc = number(highest);
        }
        // Same reasoning as `add_usage`: a model name or a service tier is not
        // a quantity, and the later frame is the one that describes the
        // response the client is about to read.
        (acc, add) => *acc = add.clone(),
    }
}

/// Write the summed usage of earlier rounds into a final response.
///
/// The client is told what the request cost in total, because that is what it
/// pays. Reporting only the last round would make a three-round retrieval look
/// like a cheap single call, which is exactly the accounting error that makes a
/// retrieval loop look free when it is not.
pub fn merge_usage(response: &mut Value, earlier: &Value) {
    if earlier.as_object().is_none_or(Map::is_empty) {
        return;
    }
    let Some(object) = response.as_object_mut() else {
        return;
    };
    match object.get_mut("usage") {
        Some(usage) if usage.is_object() => add_usage(usage, earlier),
        _ => {
            object.insert("usage".to_string(), earlier.clone());
        }
    }
}

/// A JSON number that stays an integer when the sum is one.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        return json!(value as i64);
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our call next to a client-side call that is not a `function_call` is
    /// the client's turn, not a retrieve round.
    #[test]
    fn a_custom_tool_call_beside_ours_is_the_clients_turn() {
        let ours = json!({"type":"function_call","call_id":"c1","name":RETRIEVE_TOOL_NAME,"arguments":"{}"});
        for kind in [
            "custom_tool_call",
            "local_shell_call",
            "computer_call",
            "mcp_approval_request",
        ] {
            let response = json!({"output":[ours.clone(), {"type":kind,"call_id":"c2","name":"apply_patch","input":"*** Begin Patch"}]});
            assert_eq!(
                classify(ApiFamily::OpenAiResponses, &response),
                Turn::OtherTool,
                "{kind}"
            );
        }
        // Beside something the provider already ran, ours is still ours.
        for kind in ["web_search_call", "reasoning", "message", "mcp_call"] {
            let response = json!({"output":[{"type":kind,"id":"x"}, ours.clone()]});
            assert!(
                matches!(classify(ApiFamily::OpenAiResponses, &response), Turn::Retrieve(calls) if calls.len() == 1),
                "{kind}"
            );
        }
        assert!(is_client_side_call("custom_tool_call"));
        assert!(!is_client_side_call("function_call"));
        assert!(!is_client_side_call("function_call_output"));
    }

    fn chat_response(name: &str) -> Value {
        json!({
            "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": name, "arguments": "{\"ref\":\"k_AbC123dEf456\"}"}}
            ]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 10}
        })
    }

    #[test]
    fn a_retrieve_call_in_a_later_choice_is_not_mistaken_for_an_answer() {
        // Classification read `choices[0]` only. A response whose first choice
        // is an ordinary answer and whose second holds `optscale_kompressor_retrieve`
        // was therefore `Answered`, and went to the client unchanged — handing
        // over a call to a tool the client never offered and cannot answer.
        let response = json!({"choices": [
            {"message": {"content": "here you go"}, "finish_reason": "stop"},
            {"message": {"tool_calls": [
                {"id": "private", "function": {"name": RETRIEVE_TOOL_NAME}}
            ]}, "finish_reason": "tool_calls"}
        ]});
        assert_ne!(
            classify(ApiFamily::OpenAiChat, &response),
            Turn::Answered,
            "a private call anywhere in the response is not an answered turn"
        );

        // And the strip reaches it, so nothing leaks either way.
        let mut response = response;
        assert!(strip_retrieve_calls(ApiFamily::OpenAiChat, &mut response));
        assert!(!response.to_string().contains(RETRIEVE_TOOL_NAME));
        assert!(response.to_string().contains("here you go"));
    }

    #[test]
    fn a_multi_choice_turn_is_handed_over_rather_than_retrieved_into() {
        // `n > 1` asks for several independent completions; a retrieve round
        // can only continue one. Answering into one of them silently picks a
        // branch for the caller, so the turn is handed over instead.
        let response = json!({"choices": [
            {"message": {"tool_calls": [
                {"id": "a", "function": {"name": RETRIEVE_TOOL_NAME}}
            ]}},
            {"message": {"tool_calls": [
                {"id": "b", "function": {"name": RETRIEVE_TOOL_NAME}}
            ]}}
        ]});
        assert_eq!(classify(ApiFamily::OpenAiChat, &response), Turn::OtherTool);

        // One choice is the ordinary case and still drives the retrieve loop.
        let single = json!({"choices": [
            {"message": {"tool_calls": [
                {"id": "a", "function": {"name": RETRIEVE_TOOL_NAME, "arguments": "{}"}}
            ]}}
        ]});
        assert!(matches!(
            classify(ApiFamily::OpenAiChat, &single),
            Turn::Retrieve(_)
        ));
    }

    #[test]
    fn mixed_turn_scrubbing_preserves_foreign_calls_and_text_in_every_family() {
        for (family, mut response) in [
            (
                ApiFamily::OpenAiChat,
                json!({"choices": [{"message": {"content": "hello", "tool_calls": [
                {"id": "private", "function": {"name": RETRIEVE_TOOL_NAME}},
                {"id": "client", "function": {"name": "read_file"}}
            ]}, "finish_reason": "tool_calls"}]}),
            ),
            (
                ApiFamily::OpenAiResponses,
                json!({"output": [
                    {"type": "message", "content": "hello"},
                    {"type": "function_call", "call_id": "private", "name": RETRIEVE_TOOL_NAME},
                    {"type": "function_call", "call_id": "client", "name": "read_file"}
                ]}),
            ),
            (
                ApiFamily::AnthropicMessages,
                json!({"content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "private", "name": RETRIEVE_TOOL_NAME},
                {"type": "tool_use", "id": "client", "name": "read_file"}
            ], "stop_reason": "tool_use"}),
            ),
        ] {
            assert!(strip_retrieve_calls(family, &mut response));
            assert!(!response.to_string().contains(RETRIEVE_TOOL_NAME));
            assert!(response.to_string().contains("read_file"));
            assert!(response.to_string().contains("hello"));
            assert!(!strip_retrieve_calls(family, &mut response));
        }
    }

    #[test]
    fn a_turn_of_only_our_calls_is_the_one_case_the_proxy_acts_on() {
        assert_eq!(
            classify(ApiFamily::OpenAiChat, &chat_response(RETRIEVE_TOOL_NAME)),
            Turn::Retrieve(vec![ModelToolCall {
                id: "call_1".into(),
                name: RETRIEVE_TOOL_NAME.into(),
                arguments: json!("{\"ref\":\"k_AbC123dEf456\"}"),
            }])
        );
        assert_eq!(
            classify(ApiFamily::OpenAiChat, &chat_response("read_file")),
            Turn::OtherTool
        );
        assert_eq!(
            classify(
                ApiFamily::OpenAiChat,
                &json!({"choices": [{"message": {"content": "done"}}]})
            ),
            Turn::Answered
        );
    }

    #[test]
    fn a_turn_mixing_our_call_with_another_belongs_to_the_client_whole() {
        let mixed = json!({"choices": [{"message": {"tool_calls": [
            {"id": "a", "type": "function", "function": {"name": RETRIEVE_TOOL_NAME, "arguments": "{}"}},
            {"id": "b", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}
        ]}}]});
        assert_eq!(classify(ApiFamily::OpenAiChat, &mixed), Turn::OtherTool);
    }

    #[test]
    fn each_family_names_its_calls_where_its_own_wire_puts_them() {
        let responses = json!({"output": [
            {"type": "reasoning", "id": "rs_1"},
            {"type": "function_call", "id": "fc_1", "call_id": "call_9",
             "name": RETRIEVE_TOOL_NAME, "arguments": "{\"ref\":\"k_AbC123dEf456\"}"}
        ]});
        let calls = tool_calls(ApiFamily::OpenAiResponses, &responses);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].id, "call_9",
            "the call_id is what an output quotes"
        );

        let anthropic = json!({"content": [
            {"type": "text", "text": "let me look"},
            {"type": "tool_use", "id": "toolu_1", "name": RETRIEVE_TOOL_NAME,
             "input": {"ref": "k_AbC123dEf456"}}
        ]});
        let calls = tool_calls(ApiFamily::AnthropicMessages, &anthropic);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].arguments["ref"], "k_AbC123dEf456");
    }

    #[test]
    fn the_next_round_appends_the_turn_and_its_answers_without_touching_the_base_prompt() {
        let response = chat_response(RETRIEVE_TOOL_NAME);
        let calls = tool_calls(ApiFamily::OpenAiChat, &response);
        let mut request =
            json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]});
        let results = vec![(calls[0].clone(), "{\"found\":true}".to_string())];
        assert!(append_round(
            ApiFamily::OpenAiChat,
            &mut request,
            &response,
            &results
        ));

        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"], "hi", "the base prompt is unchanged");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
    }

    #[test]
    fn a_responses_input_sent_as_a_string_becomes_a_list_before_a_turn_is_appended() {
        let response = json!({"output": [
            {"type": "function_call", "call_id": "call_9", "name": RETRIEVE_TOOL_NAME, "arguments": "{}"}
        ]});
        let calls = tool_calls(ApiFamily::OpenAiResponses, &response);
        let mut request = json!({"model": "gpt-5", "input": "summarise the log"});
        assert!(append_round(
            ApiFamily::OpenAiResponses,
            &mut request,
            &response,
            &[(calls[0].clone(), "{}".to_string())]
        ));
        let input = request["input"].as_array().unwrap();
        assert_eq!(input[0]["content"], "summarise the log");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_9");
    }

    #[test]
    fn anthropic_results_arrive_together_in_one_user_turn() {
        let response = json!({"content": [
            {"type": "tool_use", "id": "toolu_1", "name": RETRIEVE_TOOL_NAME, "input": {}},
            {"type": "tool_use", "id": "toolu_2", "name": RETRIEVE_TOOL_NAME, "input": {}}
        ]});
        let calls = tool_calls(ApiFamily::AnthropicMessages, &response);
        let mut request = json!({"messages": [{"role": "user", "content": "hi"}]});
        let results = vec![
            (calls[0].clone(), "one".to_string()),
            (calls[1].clone(), "two".to_string()),
        ];
        assert!(append_round(
            ApiFamily::AnthropicMessages,
            &mut request,
            &response,
            &results
        ));
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["content"][1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn usage_is_summed_across_rounds_including_its_nested_details() {
        let mut acc = json!({"prompt_tokens": 100, "completion_tokens": 10,
                             "prompt_tokens_details": {"cached_tokens": 90}});
        add_usage(
            &mut acc,
            &json!({"prompt_tokens": 120, "completion_tokens": 30,
                    "prompt_tokens_details": {"cached_tokens": 100}}),
        );
        assert_eq!(acc["prompt_tokens"], 220);
        assert_eq!(acc["completion_tokens"], 40);
        assert_eq!(acc["prompt_tokens_details"]["cached_tokens"], 190);

        let mut response = json!({"id": "resp", "usage": {"prompt_tokens": 5}});
        merge_usage(
            &mut response,
            &json!({"prompt_tokens": 7, "completion_tokens": 2}),
        );
        assert_eq!(response["usage"]["prompt_tokens"], 12);
        assert_eq!(response["usage"]["completion_tokens"], 2);
    }

    #[test]
    fn a_response_with_no_usage_still_reports_what_the_earlier_rounds_cost() {
        let mut response = json!({"id": "resp"});
        merge_usage(&mut response, &json!({"input_tokens": 9}));
        assert_eq!(response["usage"]["input_tokens"], 9);

        let mut untouched = json!({"id": "resp"});
        merge_usage(&mut untouched, &json!({}));
        assert!(untouched.get("usage").is_none(), "nothing to report");
    }
}
