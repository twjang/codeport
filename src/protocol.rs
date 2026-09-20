//! Loss-aware conversion for the text and function-tool subset of the three APIs.
use crate::config::Protocol;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

#[derive(Clone, Default)]
pub struct CustomTools(std::collections::BTreeMap<String, bool>, bool);

const SEARCH_TOOL: &str = "codeport_tool_search";

fn response_tools(request: &Value) -> Result<Vec<Value>> {
    let mut tools = request["tools"].as_array().cloned().unwrap_or_default();
    if let Some(items) = request["input"].as_array() {
        for item in items.iter().filter(|i| i["type"] == "tool_search_output") {
            if item["execution"] != "client" {
                bail!("server-executed tool search cannot be translated");
            }
            for tool in arr(item, "tools")? {
                if !tools
                    .iter()
                    .any(|t| t["name"] == tool["name"] && t["type"] == tool["type"])
                {
                    tools.push(tool.clone());
                }
            }
        }
    }
    Ok(tools)
}

pub fn custom_tools(request: &Value) -> Result<CustomTools> {
    let mut result = CustomTools::default();
    {
        let tools = response_tools(request)?;
        result.1 = tools
            .iter()
            .any(|t| t["type"] == "tool_search" && t["execution"] == "client");
        if result.1 && tools.iter().any(|t| t["name"] == SEARCH_TOOL) {
            bail!("tool name {SEARCH_TOOL} is reserved for translated tool search");
        }
        for tool in tools.iter().filter(|t| t["type"] == "custom") {
            let name = tool["name"]
                .as_str()
                .ok_or_else(|| anyhow!("custom tool has no name"))?;
            let grammar = tool["format"]["type"] == "grammar";
            if grammar {
                let definition = tool["format"]["definition"].as_str().unwrap_or("");
                if name != "apply_patch"
                    || tool["format"]["syntax"] != "lark"
                    || !definition.contains("*** Begin Patch")
                    || !definition.contains("*** End Patch")
                {
                    bail!("unsupported custom tool grammar for {name}; only the apply_patch patch grammar is supported");
                }
            } else if !tool["format"].is_null() && tool["format"]["type"] != "text" {
                bail!("unsupported custom tool format for {name}");
            }
            result.0.insert(name.to_owned(), grammar);
        }
    }
    Ok(result)
}

fn custom_input(arguments: &str, patch: bool) -> Result<String> {
    let args: Value = serde_json::from_str(arguments)?;
    let input = args["input"]
        .as_str()
        .ok_or_else(|| anyhow!("wrapped custom tool arguments must contain an input string"))?;
    if patch {
        validate_patch(input)?;
    }
    Ok(input.to_owned())
}

fn validate_patch(input: &str) -> Result<()> {
    let lines: Vec<_> = input.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        bail!("apply_patch input must start with *** Begin Patch and end with *** End Patch");
    }
    let mut mode = "";
    let mut files = 0;
    let mut hunk = false;
    for line in &lines[1..lines.len() - 1] {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            if path.is_empty() {
                bail!("apply_patch file path is empty");
            }
            mode = "add";
            files += 1;
            hunk = false;
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            if path.is_empty() {
                bail!("apply_patch file path is empty");
            }
            mode = "update";
            files += 1;
            hunk = false;
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            if path.is_empty() {
                bail!("apply_patch file path is empty");
            }
            mode = "delete";
            files += 1;
            hunk = false;
        } else if mode == "update" && (*line == "@@" || line.starts_with("@@ ")) {
            hunk = true;
        } else if (mode == "add" && line.starts_with('+'))
            || (mode == "update" && line.starts_with("*** Move to: ") && !hunk)
            || (mode == "update"
                && hunk
                && (line.starts_with([' ', '+', '-']) || *line == "*** End of File"))
        {
        } else {
            bail!("invalid apply_patch line: {line}");
        }
    }
    if files == 0 {
        bail!("apply_patch input contains no file operations");
    }
    Ok(())
}

fn text(v: &Value) -> Result<String> {
    if v.is_null() {
        return Ok(String::new());
    }
    if let Some(s) = v.as_str() {
        return Ok(s.into());
    }
    let mut out = String::new();
    for b in v
        .as_array()
        .ok_or_else(|| anyhow!("content must be text or an array"))?
    {
        match b["type"].as_str().unwrap_or("") {
            "text" | "input_text" | "output_text" => out.push_str(
                b["text"]
                    .as_str()
                    .ok_or_else(|| anyhow!("text block missing text"))?,
            ),
            other => bail!("unsupported content block: {other}"),
        }
    }
    Ok(out)
}
fn arr<'a>(v: &'a Value, k: &str) -> Result<&'a Vec<Value>> {
    v[k].as_array()
        .ok_or_else(|| anyhow!("{k} must be an array"))
}
fn call(id: &Value, name: &Value, args: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":args}})
}
fn parse_args(v: &Value) -> Result<Value> {
    match v.as_str() {
        Some(s) => Ok(serde_json::from_str(s)?),
        None if v.is_object() => Ok(v.clone()),
        _ => bail!("tool arguments must be JSON"),
    }
}
fn chat_stop(reason: &str) -> Result<&str> {
    match reason {
        "stop" | "tool_calls" | "length" => Ok(reason),
        _ => bail!("unsupported Chat Completions finish_reason: {reason}"),
    }
}
fn anthropic_stop(reason: &str) -> Result<&'static str> {
    match reason {
        "end_turn" | "stop_sequence" => Ok("stop"),
        "tool_use" => Ok("tool_calls"),
        "max_tokens" => Ok("length"),
        _ => bail!("unsupported Anthropic stop_reason: {reason}"),
    }
}
fn validate_incomplete(response: &Value) -> Result<()> {
    if let Some(reason) = response["incomplete_details"]["reason"].as_str() {
        if reason != "max_output_tokens" {
            bail!("unsupported Responses incomplete reason: {reason}");
        }
    }
    Ok(())
}
fn canonical_messages(v: &Value, p: Protocol) -> Result<Vec<Value>> {
    let mut out = vec![];
    match p {
        Protocol::ChatCompletions => {
            for m in arr(v, "messages")? {
                let mut m = m.clone();
                m["content"] = json!(text(&m["content"])?);
                if let Some(calls) = m["tool_calls"].as_array() {
                    for c in calls {
                        if c["type"] != "function" {
                            bail!("unsupported tool call type");
                        }
                    }
                }
                out.push(m);
            }
        }
        Protocol::Anthropic => {
            if !v["system"].is_null() {
                out.push(json!({"role":"system","content":text(&v["system"])?}));
            }
            for m in arr(v, "messages")? {
                if m["content"].is_string() {
                    out.push(m.clone());
                    continue;
                }
                let mut content = String::new();
                let mut calls = vec![];
                let mut results = vec![];
                for b in arr(m, "content")? {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => content.push_str(b["text"].as_str().unwrap_or("")),
                        "tool_use" => {
                            calls.push(call(&b["id"], &b["name"], json!(b["input"].to_string())))
                        }
                        "tool_result" => {
                            let content = text(&b["content"])?;
                            let content = if b["is_error"].as_bool() == Some(true) {
                                json!({"is_error":true,"content":content}).to_string()
                            } else {
                                content
                            };
                            results.push(json!({"role":"tool","tool_call_id":b["tool_use_id"],"content":content}));
                        }
                        t => bail!("unsupported Anthropic content block: {t}"),
                    }
                }
                out.extend(results);
                if !content.is_empty() || !calls.is_empty() {
                    let mut m = json!({"role":m["role"],"content":content});
                    if !calls.is_empty() {
                        m["tool_calls"] = json!(calls);
                    }
                    out.push(m);
                }
            }
        }
        Protocol::Responses => {
            if !v["instructions"].is_null() {
                out.push(json!({"role":"system","content":text(&v["instructions"])?}));
            }
            if v["input"].is_string() {
                out.push(json!({"role":"user","content":v["input"]}));
                return Ok(out);
            }
            for m in arr(v, "input")? {
                match m["type"].as_str().unwrap_or("message") {
                    "message"=>out.push(json!({"role":m["role"],"content":text(&m["content"])?})),
                    "function_call"=>out.push(json!({"role":"assistant","content":"","tool_calls":[call(&m["call_id"],&m["name"],m["arguments"].clone())]})),
                    "custom_tool_call"=>out.push(json!({"role":"assistant","content":"","tool_calls":[call(&m["call_id"],&m["name"],json!(json!({"input":m["input"]}).to_string()))]})),
                    "tool_search_call"=>{
                        if m["execution"]!="client" {bail!("server-executed tool search cannot be translated");}
                        let arguments=if m["arguments"].is_string(){m["arguments"].clone()}else{json!(m["arguments"].to_string())};
                        out.push(json!({"role":"assistant","content":"","tool_calls":[call(&m["call_id"],&json!(SEARCH_TOOL),arguments)]}));
                    },
                    "tool_search_output"=>out.push(json!({"role":"tool","tool_call_id":m["call_id"],"content":json!({"tools":m["tools"]}).to_string()})),
                    "function_call_output"|"custom_tool_call_output"=>out.push(json!({"role":"tool","tool_call_id":m["call_id"],"content":text(&m["output"])?})),
                    t=>bail!("unsupported Responses input item: {t}"),
                }
            }
        }
    }
    Ok(out)
}
fn encode_messages(messages: Vec<Value>, to: Protocol, out: &mut Value) -> Result<()> {
    if to == Protocol::ChatCompletions {
        let mut merged: Vec<Value> = vec![];
        for m in messages {
            if m["role"] == "assistant" && merged.last().is_some_and(|p| p["role"] == "assistant") {
                let previous = merged.last_mut().unwrap();
                let mut content = text(&previous["content"])?;
                let next = text(&m["content"])?;
                if !content.is_empty() && !next.is_empty() {
                    content.push('\n');
                }
                content.push_str(&next);
                previous["content"] = json!(content);
                if let Some(calls) = m["tool_calls"].as_array() {
                    if previous["tool_calls"].is_null() {
                        previous["tool_calls"] = json!([]);
                    }
                    previous["tool_calls"]
                        .as_array_mut()
                        .unwrap()
                        .extend(calls.clone());
                }
            } else {
                merged.push(m);
            }
        }
        out["messages"] = json!(merged);
        return Ok(());
    }
    let mut items = vec![];
    let mut system = vec![];
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        if role == "system" || role == "developer" {
            system.push(text(&m["content"])?);
            continue;
        }
        if to == Protocol::Responses {
            if role == "tool" {
                items.push(json!({"type":"function_call_output","call_id":m["tool_call_id"],"output":m["content"]}));
                continue;
            }
            let s = text(&m["content"])?;
            if !s.is_empty() {
                items.push(json!({"type":"message","role":role,"content":[{"type":if role=="assistant"{"output_text"}else{"input_text"},"text":s}]}));
            }
            if let Some(calls) = m["tool_calls"].as_array() {
                for c in calls {
                    items.push(json!({"type":"function_call","call_id":c["id"],"name":c["function"]["name"],"arguments":c["function"]["arguments"]}));
                }
            }
        } else {
            let mut blocks = vec![];
            let role = if role == "tool" {
                blocks.push(json!({"type":"tool_result","tool_use_id":m["tool_call_id"],"content":m["content"]}));
                "user"
            } else {
                let s = text(&m["content"])?;
                if !s.is_empty() {
                    blocks.push(json!({"type":"text","text":s}));
                }
                if let Some(calls) = m["tool_calls"].as_array() {
                    for c in calls {
                        blocks.push(json!({"type":"tool_use","id":c["id"],"name":c["function"]["name"],"input":parse_args(&c["function"]["arguments"])?}));
                    }
                }
                role
            };
            if let Some(last) = items.last_mut().filter(|m: &&mut Value| m["role"] == role) {
                last["content"].as_array_mut().unwrap().extend(blocks);
            } else {
                items.push(json!({"role":role,"content":blocks}));
            }
        }
    }
    if to == Protocol::Responses {
        out["input"] = json!(items);
        if !system.is_empty() {
            out["instructions"] = json!(system.join("\n\n"));
        }
    } else {
        out["messages"] = json!(items);
        if !system.is_empty() {
            out["system"] = json!(system.join("\n\n"));
        }
    }
    Ok(())
}

pub fn convert_request(mut v: Value, from: Protocol, to: Protocol) -> Result<Value> {
    if from == to {
        return Ok(v);
    }
    if from == Protocol::Responses {
        v["tools"] = json!(response_tools(&v)?);
    }
    let custom = if from == Protocol::Responses {
        custom_tools(&v)?
    } else {
        CustomTools::default()
    };
    for key in [
        "previous_response_id",
        "conversation",
        "response_format",
        "output_config",
        "modalities",
        "audio",
        "prediction",
        "top_k",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "seed",
        "logprobs",
        "top_logprobs",
        "functions",
        "function_call",
        "verbosity",
    ] {
        if !v[key].is_null() {
            bail!("{key} is unsupported when converting protocols");
        }
    }
    if v["truncation"]
        .as_str()
        .is_some_and(|mode| mode != "disabled")
    {
        bail!("automatic context truncation cannot be preserved across protocols");
    }
    if v["background"].as_bool() == Some(true) {
        bail!("background Responses requests cannot be preserved across protocols");
    }
    if v["n"].as_u64().unwrap_or(1) != 1 {
        bail!("multiple response choices are unsupported");
    }
    let mut out = json!({});
    if !v["thinking"].is_null() && v["thinking"]["type"] != "disabled" {
        bail!("Anthropic extended thinking cannot be translated to this backend protocol");
    }
    if v["context_management"]["edits"]
        .as_array()
        .is_some_and(|edits| !edits.is_empty())
    {
        bail!("Anthropic context-management edits cannot be translated across protocols");
    }
    let effort = v["reasoning"]["effort"]
        .as_str()
        .or_else(|| v["reasoning_effort"].as_str());
    if let Some(effort) = effort {
        if to == Protocol::Anthropic {
            if effort != "none" {
                bail!("reasoning effort cannot be translated to Anthropic; configure the agent with reasoning disabled");
            }
        } else if to == Protocol::Responses {
            out["reasoning"] = json!({"effort":effort});
        } else {
            out["reasoning_effort"] = json!(effort);
        }
    }
    if v["reasoning"]["summary"]
        .as_str()
        .is_some_and(|s| s != "none")
    {
        bail!("reasoning summaries cannot be preserved across protocols");
    }
    if !v["text"]["format"].is_null() && v["text"]["format"]["type"] != "text" {
        bail!("structured text output is unsupported across protocols");
    }
    if let Some(verbosity) = v["text"]["verbosity"].as_str() {
        if to == Protocol::ChatCompletions {
            out["verbosity"] = json!(verbosity);
        } else if verbosity != "medium" {
            bail!("text verbosity cannot be preserved for Anthropic");
        }
    }
    for key in ["model", "stream", "temperature", "top_p"] {
        if !v[key].is_null() {
            out[key] = v[key].clone();
        }
    }
    let max = ["max_output_tokens", "max_completion_tokens", "max_tokens"]
        .iter()
        .find_map(|k| v.get(k))
        .cloned();
    if let Some(max) = max {
        out[if to == Protocol::Responses {
            "max_output_tokens"
        } else {
            "max_tokens"
        }] = max;
    } else if to == Protocol::Anthropic {
        out["max_tokens"] = json!(8192);
    }
    if !v["stop"].is_null() || !v["stop_sequences"].is_null() {
        if to == Protocol::Responses {
            bail!("stop sequences are unsupported by Responses");
        }
        let s = if from == Protocol::Anthropic {
            v["stop_sequences"].clone()
        } else {
            v["stop"].clone()
        };
        out[if to == Protocol::Anthropic {
            "stop_sequences"
        } else {
            "stop"
        }] = if s.is_string() { json!([s]) } else { s };
    }
    encode_messages(canonical_messages(&v, from)?, to, &mut out)?;
    if let Some(tools) = v["tools"].as_array() {
        let mut converted = vec![];
        for t in tools {
            if from == Protocol::Responses
                && t["type"] == "tool_search"
                && t["execution"] == "client"
            {
                let description = t["description"]
                    .as_str()
                    .unwrap_or("Search for additional tools available to the coding agent.");
                if to == Protocol::Anthropic {
                    converted.push(json!({"name":SEARCH_TOOL,"description":description,"input_schema":t["parameters"]}));
                } else {
                    converted.push(json!({"type":"function","function":{"name":SEARCH_TOOL,"description":description,"parameters":t["parameters"]}}));
                }
                continue;
            }
            if from == Protocol::Responses && t["type"] == "custom" {
                let name = t["name"].as_str().unwrap();
                let mut description = format!(
                    "{}\nPass the complete raw tool input as the input string.",
                    t["description"].as_str().unwrap_or("")
                );
                if custom.0[name] {
                    description.push_str("\nThe input must be a valid apply_patch patch starting with *** Begin Patch and ending with *** End Patch.\n");
                    description.push_str(t["format"]["definition"].as_str().unwrap_or(""));
                }
                let schema = json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false});
                if to == Protocol::Anthropic {
                    converted
                        .push(json!({"name":name,"description":description,"input_schema":schema}));
                } else {
                    converted.push(json!({"type":"function","function":{"name":name,"description":description,"parameters":schema}}));
                }
                continue;
            }
            let f = if from == Protocol::ChatCompletions {
                if t["type"] != "function" {
                    bail!("unsupported built-in tool");
                }
                &t["function"]
            } else {
                t
            };
            if from == Protocol::Responses && t["type"] != "function" {
                bail!("unsupported Responses built-in tool: {}", t["type"]);
            }
            if from == Protocol::Anthropic && !t["type"].is_null() && t["type"] != "custom" {
                bail!("unsupported Anthropic built-in tool");
            }
            if f["strict"] == true && to == Protocol::Anthropic {
                bail!("strict function schemas cannot be preserved for this backend protocol");
            }
            let schema = if from == Protocol::Anthropic {
                &f["input_schema"]
            } else {
                &f["parameters"]
            };
            let strict = f.get("strict").cloned();
            let mut f = json!({"name":f["name"],"description":f["description"].as_str().unwrap_or(""),"parameters":schema});
            if to != Protocol::Anthropic {
                if let Some(strict) = strict {
                    f["strict"] = strict;
                }
            }
            if to == Protocol::Anthropic {
                let schema = f.as_object_mut().unwrap().remove("parameters").unwrap();
                f["input_schema"] = schema;
                converted.push(f);
            } else if to == Protocol::ChatCompletions {
                converted.push(json!({"type":"function","function":f}));
            } else {
                f["type"] = json!("function");
                converted.push(f);
            }
        }
        out["tools"] = json!(converted);
    }
    if let Some(choice) = v.get("tool_choice") {
        let kind = choice
            .as_str()
            .or_else(|| choice["type"].as_str())
            .unwrap_or("");
        let kind = match kind {
            "any" => "required",
            "tool" | "function" | "custom" => "function",
            x => x,
        };
        let name = if from == Protocol::ChatCompletions {
            &choice["function"]["name"]
        } else {
            &choice["name"]
        };
        out["tool_choice"] = if to == Protocol::Anthropic {
            match kind {
                "auto" | "none" => json!({"type":kind}),
                "required" => json!({"type":"any"}),
                "function" => json!({"type":"tool","name":name}),
                _ => bail!("unsupported tool choice"),
            }
        } else if kind == "function" {
            if to == Protocol::ChatCompletions {
                json!({"type":"function","function":{"name":name}})
            } else {
                json!({"type":"function","name":name})
            }
        } else {
            json!(kind)
        };
    }
    if let Some(parallel) = v.get("parallel_tool_calls") {
        if to == Protocol::Anthropic {
            if out["tool_choice"].is_null() {
                out["tool_choice"] = json!({"type":"auto"});
            }
            out["tool_choice"]["disable_parallel_tool_use"] =
                json!(!parallel.as_bool().unwrap_or(true));
        } else {
            out["parallel_tool_calls"] = parallel.clone();
        }
    }
    if from == Protocol::Anthropic {
        if let Some(disable) = v["tool_choice"]["disable_parallel_tool_use"].as_bool() {
            out["parallel_tool_calls"] = json!(!disable);
        }
    }
    Ok(out)
}

pub fn convert_response(v: Value, from: Protocol, to: Protocol) -> Result<Value> {
    if from == to {
        return Ok(v);
    }
    if !v["error"].is_null() {
        bail!("upstream API error: {}", v["error"]);
    }
    if from == Protocol::ChatCompletions {
        let choices = arr(&v, "choices")?;
        if choices.len() != 1 {
            bail!("expected exactly one completion choice");
        }
        for key in [
            "refusal",
            "reasoning_content",
            "reasoning",
            "audio",
            "function_call",
        ] {
            if !choices[0]["message"][key].is_null() {
                bail!("{key} content cannot be preserved across protocols");
            }
        }
        if !choices[0]["logprobs"].is_null() {
            bail!("response logprobs cannot be preserved across protocols");
        }
        if choices[0]["message"]["annotations"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            bail!("response annotations cannot be preserved across protocols");
        }
        chat_stop(choices[0]["finish_reason"].as_str().unwrap_or("missing"))?;
        text(&choices[0]["message"]["content"])?;
    }
    if from == Protocol::Responses
        && !matches!(v["status"].as_str(), Some("completed" | "incomplete"))
    {
        bail!(
            "upstream Responses request did not complete: {}",
            v["status"]
        );
    }
    let (message, reason, input, output) = match from {
        Protocol::ChatCompletions => (
            v["choices"][0]["message"].clone(),
            v["choices"][0]["finish_reason"]
                .as_str()
                .unwrap_or("stop")
                .to_string(),
            v["usage"]["prompt_tokens"].clone(),
            v["usage"]["completion_tokens"].clone(),
        ),
        Protocol::Anthropic => {
            let msgs = canonical_messages(
                &json!({"messages":[{"role":"assistant","content":v["content"]}]}),
                from,
            )?;
            (
                msgs.into_iter()
                    .next()
                    .unwrap_or(json!({"role":"assistant","content":""})),
                anthropic_stop(v["stop_reason"].as_str().unwrap_or("missing"))?.into(),
                v["usage"]["input_tokens"].clone(),
                v["usage"]["output_tokens"].clone(),
            )
        }
        Protocol::Responses => {
            let msgs = canonical_messages(&json!({"input":v["output"]}), from)?;
            let mut content = String::new();
            let mut calls = vec![];
            for m in msgs {
                content.push_str(&text(&m["content"])?);
                if let Some(c) = m["tool_calls"].as_array() {
                    calls.extend(c.clone());
                }
            }
            let reason = if v["status"] == "incomplete" {
                validate_incomplete(&v)?;
                "length"
            } else if calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            };
            (
                json!({"role":"assistant","content":content,"tool_calls":calls}),
                reason.into(),
                v["usage"]["input_tokens"].clone(),
                v["usage"]["output_tokens"].clone(),
            )
        }
    };
    let input = input.as_u64().unwrap_or(0);
    let output = output.as_u64().unwrap_or(0);
    match to {
        Protocol::ChatCompletions => Ok(
            json!({"id":v["id"],"object":"chat.completion","created":v["created_at"].as_u64().unwrap_or(0),"model":v["model"],"choices":[{"index":0,"message":message,"finish_reason":reason}],"usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output}}),
        ),
        Protocol::Anthropic => {
            let mut encoded = json!({});
            encode_messages(vec![message], to, &mut encoded)?;
            Ok(
                json!({"id":v["id"],"type":"message","role":"assistant","model":v["model"],"content":encoded["messages"][0]["content"],"stop_reason":match reason.as_str(){"tool_calls"=>"tool_use","length"=>"max_tokens",_=>"end_turn"},"stop_sequence":null,"usage":{"input_tokens":input,"output_tokens":output}}),
            )
        }
        Protocol::Responses => {
            let mut encoded = json!({});
            encode_messages(vec![message], to, &mut encoded)?;
            let mut items = encoded["input"].as_array().unwrap().clone();
            for (i, item) in items.iter_mut().enumerate() {
                item["id"] = json!(format!("lc_item_{i}"));
                item["status"] = json!("completed");
                if let Some(content) = item["content"].as_array_mut() {
                    for block in content {
                        block["annotations"] = json!([]);
                    }
                }
            }
            Ok(
                json!({"id":v["id"],"object":"response","created_at":0,"status":if reason=="length"{"incomplete"}else{"completed"},"model":v["model"],"output":items,"error":null,"incomplete_details":if reason=="length"{json!({"reason":"max_output_tokens"})}else{Value::Null},"usage":{"input_tokens":input,"output_tokens":output,"total_tokens":input+output}}),
            )
        }
    }
}

pub fn convert_response_with_tools(
    v: Value,
    from: Protocol,
    to: Protocol,
    custom: &CustomTools,
) -> Result<Value> {
    let mut response = convert_response(v, from, to)?;
    if to == Protocol::Responses && from != to {
        if let Some(items) = response["output"].as_array_mut() {
            for item in items {
                if custom.1 && item["name"] == SEARCH_TOOL {
                    item["arguments"] = parse_args(&item["arguments"])?;
                    item["type"] = json!("tool_search_call");
                    item["execution"] = json!("client");
                    item.as_object_mut().unwrap().remove("name");
                    continue;
                }
                if let Some(patch) = item["name"].as_str().and_then(|name| custom.0.get(name)) {
                    let input = custom_input(item["arguments"].as_str().unwrap_or(""), *patch)?;
                    item.as_object_mut().unwrap().remove("arguments");
                    item["type"] = json!("custom_tool_call");
                    item["input"] = json!(input);
                }
            }
        }
    }
    Ok(response)
}

#[derive(Default)]
struct Block {
    index: usize,
    tool_index: usize,
    id: String,
    name: String,
    text: String,
    tool: bool,
    initial_arguments: Option<String>,
}
/// Converts complete SSE frames. Output strings already include SSE framing.
pub struct StreamConverter {
    from: Protocol,
    to: Protocol,
    started: bool,
    ended: bool,
    id: String,
    model: String,
    blocks: std::collections::BTreeMap<usize, Block>,
    sequence: u64,
    input: u64,
    output: u64,
    pending_reason: Option<String>,
    custom: CustomTools,
}
impl StreamConverter {
    pub fn new(from: Protocol, to: Protocol) -> Self {
        Self {
            from,
            to,
            started: false,
            ended: false,
            id: format!("lc_{}", uuid::Uuid::new_v4().simple()),
            model: String::new(),
            blocks: Default::default(),
            sequence: 0,
            input: 0,
            output: 0,
            pending_reason: None,
            custom: CustomTools::default(),
        }
    }
    pub fn with_custom_tools(mut self, custom: CustomTools) -> Self {
        self.custom = custom;
        self
    }
    fn emit(&mut self, mut v: Value) -> String {
        if self.to == Protocol::Responses {
            v["sequence_number"] = json!(self.sequence);
            self.sequence += 1;
        }
        if self.to == Protocol::ChatCompletions {
            format!("data: {v}\n\n")
        } else {
            format!(
                "event: {}\ndata: {v}\n\n",
                v["type"].as_str().unwrap_or("error")
            )
        }
    }
    fn chunk(&self, delta: Value, finish: Value) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }
    fn response(&self, status: &str) -> Value {
        json!({"id":self.id,"object":"response","created_at":0,"status":status,"model":self.model,"output":[],"error":null,"incomplete_details":null,"usage":{"input_tokens":self.input,"output_tokens":self.output,"total_tokens":self.input+self.output}})
    }
    fn start(&mut self, out: &mut Vec<String>) {
        if self.started {
            return;
        }
        self.started = true;
        match self.to{
        Protocol::ChatCompletions=>{let v=self.chunk(json!({"role":"assistant"}),Value::Null);out.push(self.emit(v));},
        Protocol::Anthropic=>out.push(self.emit(json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":self.input,"output_tokens":0}}}))),
        Protocol::Responses=>{out.push(self.emit(json!({"type":"response.created","response":self.response("in_progress")})));out.push(self.emit(json!({"type":"response.in_progress","response":self.response("in_progress")})));}
    }
    }
    fn block(
        &mut self,
        key: usize,
        id: Option<&str>,
        name: Option<&str>,
        out: &mut Vec<String>,
    ) -> Result<()> {
        if self.blocks.contains_key(&key) {
            return Ok(());
        }
        self.start(out);
        let b = Block {
            index: self.blocks.len(),
            tool_index: self.blocks.values().filter(|b| b.tool).count(),
            id: id
                .map(str::to_owned)
                .unwrap_or_else(|| format!("lc_item_{}", self.blocks.len())),
            name: name.unwrap_or("").into(),
            tool: name.is_some(),
            ..Default::default()
        };
        let v = match self.to {
            Protocol::ChatCompletions => {
                if b.tool {
                    Some(self.chunk(json!({"tool_calls":[{"index":b.tool_index,"id":b.id,"type":"function","function":{"name":b.name,"arguments":""}}]}),Value::Null))
                } else {
                    None
                }
            }
            Protocol::Anthropic => {
                if b.index == 0 && !b.tool {
                    Some(
                        json!({"type":"content_block_start","index":b.index,"content_block":{"type":"text","text":""}}),
                    )
                } else {
                    None
                }
            }
            Protocol::Responses => Some(
                json!({"type":"response.output_item.added","output_index":b.index,"item":if b.tool{if self.custom.1 && b.name==SEARCH_TOOL{json!({"id":format!("fc_{}",b.index),"type":"tool_search_call","call_id":b.id,"execution":"client","arguments":{},"status":"in_progress"})}else if self.custom.0.contains_key(&b.name){json!({"id":format!("fc_{}",b.index),"type":"custom_tool_call","call_id":b.id,"name":b.name,"input":"","status":"in_progress"})}else{json!({"id":format!("fc_{}",b.index),"type":"function_call","call_id":b.id,"name":b.name,"arguments":"","status":"in_progress"})}}else{json!({"id":b.id,"type":"message","role":"assistant","content":[],"status":"in_progress"})}}),
            ),
        };
        if let Some(v) = v {
            out.push(self.emit(v));
        }
        if self.to == Protocol::Responses && !b.tool {
            out.push(self.emit(json!({"type":"response.content_part.added","item_id":b.id,"output_index":b.index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})));
        }
        self.blocks.insert(key, b);
        Ok(())
    }
    fn delta(&mut self, key: usize, s: &str, out: &mut Vec<String>) -> Result<()> {
        let b = self
            .blocks
            .get_mut(&key)
            .ok_or_else(|| anyhow!("stream delta without content block"))?;
        b.text.push_str(s);
        // OpenAI allows interleaved tool deltas; Anthropic requires sequential
        // content blocks. Stream leading prose immediately, buffer later blocks.
        if self.to == Protocol::Anthropic && (b.tool || b.index > 0) {
            return Ok(());
        }
        if self.to == Protocol::Responses
            && b.tool
            && (self.custom.0.contains_key(&b.name) || (self.custom.1 && b.name == SEARCH_TOOL))
        {
            return Ok(());
        }
        let v = match self.to {
            Protocol::ChatCompletions => {
                let d = if b.tool {
                    json!({"tool_calls":[{"index":b.tool_index,"function":{"arguments":s}}]})
                } else {
                    json!({"content":s})
                };
                self.chunk(d, Value::Null)
            }
            Protocol::Anthropic => {
                json!({"type":"content_block_delta","index":b.index,"delta":if b.tool{json!({"type":"input_json_delta","partial_json":s})}else{json!({"type":"text_delta","text":s})}})
            }
            Protocol::Responses => {
                if b.tool {
                    json!({"type":"response.function_call_arguments.delta","item_id":format!("fc_{}",b.index),"output_index":b.index,"delta":s})
                } else {
                    json!({"type":"response.output_text.delta","item_id":b.id,"output_index":b.index,"content_index":0,"delta":s,"logprobs":[]})
                }
            }
        };
        out.push(self.emit(v));
        Ok(())
    }
    fn end(&mut self, reason: &str, out: &mut Vec<String>) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        chat_stop(reason)?;
        self.start(out);
        let missing: Vec<_> = self
            .blocks
            .iter()
            .filter(|(_, b)| b.tool && b.text.is_empty())
            .map(|(key, b)| {
                (
                    *key,
                    b.initial_arguments.clone().unwrap_or_else(|| "{}".into()),
                )
            })
            .collect();
        for (key, args) in missing {
            self.delta(key, &args, out)?;
        }
        let mut items = vec![];
        let mut events = vec![];
        let mut blocks: Vec<_> = self.blocks.values().collect();
        blocks.sort_by_key(|b| b.index);
        for b in blocks {
            if b.tool {
                let _: Value = serde_json::from_str(&b.text)
                    .map_err(|e| anyhow!("invalid streamed tool arguments: {e}"))?;
            }
            match self.to {
                Protocol::Anthropic => {
                    if b.tool || b.index > 0 {
                        events.push(json!({"type":"content_block_start","index":b.index,"content_block":if b.tool{json!({"type":"tool_use","id":b.id,"name":b.name,"input":{}})}else{json!({"type":"text","text":""})}}));
                        events.push(json!({"type":"content_block_delta","index":b.index,"delta":if b.tool{json!({"type":"input_json_delta","partial_json":b.text})}else{json!({"type":"text_delta","text":b.text})}}));
                    }
                    events.push(json!({"type":"content_block_stop","index":b.index}))
                }
                Protocol::Responses => {
                    let item = if b.tool && self.custom.1 && b.name == SEARCH_TOOL {
                        json!({"id":format!("fc_{}",b.index),"type":"tool_search_call","call_id":b.id,"execution":"client","arguments":parse_args(&json!(b.text))?,"status":"completed"})
                    } else if let Some(patch) = self.custom.0.get(&b.name).filter(|_| b.tool) {
                        let input = custom_input(&b.text, *patch)?;
                        events.push(json!({"type":"response.custom_tool_call_input.delta","item_id":format!("fc_{}",b.index),"output_index":b.index,"delta":input}));
                        events.push(json!({"type":"response.custom_tool_call_input.done","item_id":format!("fc_{}",b.index),"output_index":b.index,"input":input}));
                        json!({"id":format!("fc_{}",b.index),"type":"custom_tool_call","call_id":b.id,"name":b.name,"input":input,"status":"completed"})
                    } else if b.tool {
                        events.push(json!({"type":"response.function_call_arguments.done","item_id":format!("fc_{}",b.index),"output_index":b.index,"arguments":b.text,"name":b.name}));
                        json!({"id":format!("fc_{}",b.index),"type":"function_call","call_id":b.id,"name":b.name,"arguments":b.text,"status":"completed"})
                    } else {
                        let part = json!({"type":"output_text","text":b.text,"annotations":[],"logprobs":[]});
                        events.push(json!({"type":"response.output_text.done","item_id":b.id,"output_index":b.index,"content_index":0,"text":b.text,"logprobs":[]}));
                        events.push(json!({"type":"response.content_part.done","item_id":b.id,"output_index":b.index,"content_index":0,"part":part}));
                        json!({"id":b.id,"type":"message","role":"assistant","content":[part],"status":"completed"})
                    };
                    events.push(json!({"type":"response.output_item.done","output_index":b.index,"item":item}));
                    items.push(item);
                }
                _ => {}
            }
        }
        for e in events {
            out.push(self.emit(e));
        }
        match self.to {
            Protocol::ChatCompletions => {
                out.push(self.emit(self.chunk(json!({}), json!(reason))));
                out.push(self.emit(json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,"choices":[],"usage":{"prompt_tokens":self.input,"completion_tokens":self.output,"total_tokens":self.input+self.output}})));
                out.push("data: [DONE]\n\n".into());
            }
            Protocol::Anthropic => {
                out.push(self.emit(json!({"type":"message_delta","delta":{"stop_reason":match reason{"tool_calls"=>"tool_use","length"=>"max_tokens",_=>"end_turn"},"stop_sequence":null},"usage":{"output_tokens":self.output,"input_tokens":self.input}})));
                out.push(self.emit(json!({"type":"message_stop"})));
            }
            Protocol::Responses => {
                let status = if reason == "length" {
                    "incomplete"
                } else {
                    "completed"
                };
                let mut r = self.response(status);
                r["output"] = json!(items);
                if reason == "length" {
                    r["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                out.push(self.emit(json!({"type":format!("response.{status}"),"response":r})));
            }
        }
        self.ended = true;
        Ok(())
    }
    pub fn push(&mut self, event: &str, data: &str) -> Result<Vec<String>> {
        if self.from == self.to {
            return Ok(vec![if event.is_empty() {
                format!("data: {data}\n\n")
            } else {
                format!("event: {event}\ndata: {data}\n\n")
            }]);
        }
        let mut out = vec![];
        if data == "[DONE]" {
            if let Some(reason) = self.pending_reason.take() {
                self.end(&reason, &mut out)?;
            }
            if !self.ended {
                bail!("upstream stream ended without a finish reason");
            }
            return Ok(out);
        }
        let v: Value = serde_json::from_str(data)?;
        if !v["error"].is_null() || v["type"] == "error" || v["type"] == "response.failed" {
            bail!("upstream streaming API error: {v}");
        }
        if let Some(model) = v["model"].as_str() {
            self.model = model.into();
        }
        match self.from {
            Protocol::ChatCompletions => {
                if !v["usage"].is_null() {
                    self.input = v["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
                    self.output = v["usage"]["completion_tokens"].as_u64().unwrap_or(0);
                }
                if let Some(choices) = v["choices"].as_array() {
                    for c in choices {
                        if c["index"].as_u64().unwrap_or(0) != 0 {
                            bail!("multiple streamed choices unsupported");
                        }
                        let d = &c["delta"];
                        for key in [
                            "reasoning_content",
                            "reasoning",
                            "refusal",
                            "audio",
                            "function_call",
                        ] {
                            if !d[key].is_null() {
                                bail!("unsupported {key} stream delta");
                            }
                        }
                        if !c["logprobs"].is_null() {
                            bail!("streamed logprobs cannot be preserved across protocols");
                        }
                        if let Some(s) = d["content"].as_str() {
                            self.block(0, None, None, &mut out)?;
                            self.delta(0, s, &mut out)?;
                        }
                        if let Some(calls) = d["tool_calls"].as_array() {
                            for c in calls {
                                let key = c["index"].as_u64().unwrap_or(0) as usize + 1;
                                if !self.blocks.contains_key(&key)
                                    && c["function"]["name"].as_str().is_none()
                                {
                                    bail!("tool stream must start with a tool name");
                                }
                                self.block(
                                    key,
                                    c["id"].as_str(),
                                    c["function"]["name"].as_str(),
                                    &mut out,
                                )?;
                                if let Some(s) = c["function"]["arguments"].as_str() {
                                    self.delta(key, s, &mut out)?;
                                }
                            }
                        }
                        if let Some(reason) = c["finish_reason"].as_str() {
                            chat_stop(reason)?;
                            self.pending_reason = Some(reason.to_string());
                        }
                    }
                }
            }
            Protocol::Anthropic => {
                let key = v["index"].as_u64().unwrap_or(0) as usize;
                match v["type"].as_str().unwrap_or(event) {
                    "message_start" => {
                        self.model = v["message"]["model"].as_str().unwrap_or("").into();
                        self.input = v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
                        self.start(&mut out);
                    }
                    "content_block_start" => {
                        let b = &v["content_block"];
                        match b["type"].as_str() {
                            Some("text") => {
                                self.block(key, None, None, &mut out)?;
                                if let Some(s) = b["text"].as_str() {
                                    if !s.is_empty() {
                                        self.delta(key, s, &mut out)?;
                                    }
                                }
                            }
                            Some("tool_use") => {
                                self.block(key, b["id"].as_str(), b["name"].as_str(), &mut out)?;
                                if let Some(input) = b.get("input") {
                                    let object = input.as_object().ok_or_else(|| {
                                        anyhow!("tool_use input must be an object")
                                    })?;
                                    if !object.is_empty() {
                                        self.blocks.get_mut(&key).unwrap().initial_arguments =
                                            Some(input.to_string());
                                    }
                                }
                            }
                            _ => bail!("unsupported streamed Anthropic block: {}", b["type"]),
                        }
                    }
                    "content_block_delta" => match v["delta"]["type"].as_str() {
                        Some("text_delta") => {
                            self.delta(key, v["delta"]["text"].as_str().unwrap_or(""), &mut out)?
                        }
                        Some("input_json_delta") => {
                            if self
                                .blocks
                                .get(&key)
                                .is_some_and(|b| b.initial_arguments.is_some())
                            {
                                bail!("tool_use stream combines complete initial input and argument deltas");
                            }
                            self.delta(
                                key,
                                v["delta"]["partial_json"].as_str().unwrap_or(""),
                                &mut out,
                            )?;
                        }
                        _ => bail!("unsupported Anthropic delta: {}", v["delta"]["type"]),
                    },
                    "message_delta" => {
                        self.output = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
                        if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                            self.end(anthropic_stop(reason)?, &mut out)?;
                        }
                    }
                    "message_stop" | "content_block_stop" | "ping" => {}
                    t => bail!("unsupported Anthropic stream event: {t}"),
                }
            }
            Protocol::Responses => {
                let key = v["output_index"].as_u64().unwrap_or(0) as usize;
                match v["type"].as_str().unwrap_or(event) {
                    "response.created" | "response.in_progress" => {
                        self.model = v["response"]["model"].as_str().unwrap_or("").into();
                        self.start(&mut out);
                    }
                    "response.output_item.added" => {
                        let b = &v["item"];
                        match b["type"].as_str() {
                            Some("message") => {}
                            Some("function_call") => self.block(
                                key,
                                b["call_id"].as_str(),
                                b["name"].as_str(),
                                &mut out,
                            )?,
                            t => bail!("unsupported Responses output item: {t:?}"),
                        }
                    }
                    "response.content_part.added" => {
                        if v["part"]["type"] != "output_text" {
                            bail!("unsupported Responses content part");
                        }
                        self.block(key, None, None, &mut out)?;
                    }
                    "response.output_text.delta" => {
                        self.block(key, None, None, &mut out)?;
                        self.delta(key, v["delta"].as_str().unwrap_or(""), &mut out)?;
                    }
                    "response.function_call_arguments.delta" => {
                        self.delta(key, v["delta"].as_str().unwrap_or(""), &mut out)?
                    }
                    "response.completed" | "response.incomplete" => {
                        if v["type"] == "response.incomplete" {
                            validate_incomplete(&v["response"])?;
                        }
                        self.input = v["response"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
                        self.output = v["response"]["usage"]["output_tokens"]
                            .as_u64()
                            .unwrap_or(0);
                        let reason = if v["type"] == "response.incomplete" {
                            "length"
                        } else if self.blocks.values().any(|b| b.tool) {
                            "tool_calls"
                        } else {
                            "stop"
                        };
                        self.end(reason, &mut out)?;
                    }
                    "response.output_text.done"
                    | "response.function_call_arguments.done"
                    | "response.content_part.done"
                    | "response.output_item.done" => {}
                    t => bail!("unsupported Responses stream event: {t}"),
                }
            }
        }
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<Vec<String>> {
        let mut out = vec![];
        if let Some(reason) = self.pending_reason.take() {
            self.end(&reason, &mut out)?;
        }
        if self.from != self.to && !self.ended {
            bail!("upstream stream truncated before completion");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unmapped_semantic_controls_fail_but_operational_metadata_is_allowed() {
        let base = json!({"messages":[{"role":"user","content":"hi"}],"metadata":{"trace":"test"},"client_metadata":{},"store":false,"prompt_cache_key":"session","stream_options":{"include_usage":true}});
        for (key, value) in [
            ("top_k", json!(10)),
            ("presence_penalty", json!(0.5)),
            ("frequency_penalty", json!(0.5)),
            ("logit_bias", json!({"1":5})),
            ("seed", json!(12)),
            ("logprobs", json!(true)),
            ("top_logprobs", json!(3)),
            ("truncation", json!("auto")),
            ("background", json!(true)),
        ] {
            let mut request = base.clone();
            request[key] = value;
            assert!(
                convert_request(
                    request.clone(),
                    Protocol::ChatCompletions,
                    Protocol::Anthropic
                )
                .is_err(),
                "{key} was discarded"
            );
            assert_eq!(
                convert_request(
                    request.clone(),
                    Protocol::ChatCompletions,
                    Protocol::ChatCompletions
                )
                .unwrap(),
                request
            );
        }
        assert!(convert_request(base, Protocol::ChatCompletions, Protocol::Anthropic).is_ok());
    }
    #[test]
    fn responses_reject_semantic_content_and_unknown_stop_reasons() {
        let base = json!({"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]});
        for key in ["refusal", "reasoning_content", "reasoning", "audio"] {
            let mut response = base.clone();
            response["choices"][0]["message"][key] = json!("extra");
            assert!(
                convert_response(response, Protocol::ChatCompletions, Protocol::Responses).is_err()
            );
        }
        let mut filtered = base.clone();
        filtered["choices"][0]["finish_reason"] = json!("content_filter");
        assert!(
            convert_response(filtered, Protocol::ChatCompletions, Protocol::Anthropic).is_err()
        );
        let mut multiple = base.clone();
        multiple["choices"]
            .as_array_mut()
            .unwrap()
            .push(base["choices"][0].clone());
        assert!(
            convert_response(multiple, Protocol::ChatCompletions, Protocol::Anthropic).is_err()
        );
        for reason in ["refusal", "pause_turn", "new_future_reason"] {
            assert!(convert_response(
                json!({"content":[{"type":"text","text":"hi"}],"stop_reason":reason}),
                Protocol::Anthropic,
                Protocol::ChatCompletions
            )
            .is_err());
            let mut stream = StreamConverter::new(Protocol::Anthropic, Protocol::ChatCompletions);
            assert!(stream
                .push(
                    "",
                    &json!({"type":"message_delta","delta":{"stop_reason":reason}}).to_string()
                )
                .is_err());
        }
        let mut stream = StreamConverter::new(Protocol::ChatCompletions, Protocol::Responses);
        assert!(stream
            .push(
                "",
                &json!({"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]})
                    .to_string()
            )
            .is_err());
        assert!(convert_response(json!({"status":"incomplete","output":[],"incomplete_details":{"reason":"content_filter"}}),Protocol::Responses,Protocol::ChatCompletions).is_err());
    }
    #[test]
    fn tool_roundtrip() {
        let v = json!({"model":"x","messages":[{"role":"system","content":"help"},{"role":"user","content":"read"},{"role":"assistant","content":"","tool_calls":[{"id":"call_a","type":"function","function":{"name":"read","arguments":"{\"file\":\"x\"}"}}]},{"role":"tool","tool_call_id":"call_a","content":"contents"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]});
        for p in [Protocol::Anthropic, Protocol::Responses] {
            let a = convert_request(v.clone(), Protocol::ChatCompletions, p).unwrap();
            let b = convert_request(a, p, Protocol::ChatCompletions).unwrap();
            assert_eq!(b["messages"][2]["tool_calls"][0]["id"], "call_a");
            assert_eq!(b["messages"][3]["content"], "contents");
        }
    }
    #[test]
    fn rejects_images() {
        assert!(convert_request(json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]}),Protocol::ChatCompletions,Protocol::Anthropic).is_err());
    }
    #[test]
    fn streams_text_and_tools() {
        for to in [Protocol::Anthropic, Protocol::Responses] {
            let mut s = StreamConverter::new(Protocol::ChatCompletions, to);
            let mut frames = vec![];
            for d in [
                json!({"content":"Hi"}),
                json!({"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":"{\"x\":"}}]}),
                json!({"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}),
            ] {
                frames.extend(
                    s.push(
                        "",
                        &json!({"choices":[{"index":0,"delta":d,"finish_reason":null}]})
                            .to_string(),
                    )
                    .unwrap(),
                );
            }
            frames.extend(
                s.push(
                    "",
                    &json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
                        .to_string(),
                )
                .unwrap(),
            );
            frames.extend(s.finish().unwrap());
            assert!(frames.join("").contains("call_a"));
        }
    }
    #[test]
    fn detects_truncated_stream() {
        let mut s = StreamConverter::new(Protocol::ChatCompletions, Protocol::Anthropic);
        assert!(s.finish().is_err());
    }
    #[test]
    fn response_conversion_matrix_preserves_tools() {
        let chat = json!({"id":"r1","model":"m","choices":[{"message":{"role":"assistant","content":"Reading","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5}});
        for from in [
            Protocol::ChatCompletions,
            Protocol::Anthropic,
            Protocol::Responses,
        ] {
            let source = convert_response(chat.clone(), Protocol::ChatCompletions, from).unwrap();
            for to in [
                Protocol::ChatCompletions,
                Protocol::Anthropic,
                Protocol::Responses,
            ] {
                let target = convert_response(source.clone(), from, to).unwrap();
                let recovered = convert_response(target, to, Protocol::ChatCompletions).unwrap();
                assert_eq!(recovered["choices"][0]["message"]["content"], "Reading");
                assert_eq!(
                    recovered["choices"][0]["message"]["tool_calls"][0]["id"],
                    "call_1"
                );
                assert_eq!(recovered["usage"]["prompt_tokens"], 12);
            }
        }
    }
    #[test]
    fn accepts_codex_plain_text_controls() {
        let request = json!({"model":"local","input":[{"role":"user","content":[{"type":"input_text","text":"Hello"}]}],"stream":true,"store":false,"include":["reasoning.encrypted_content"],"reasoning":{"effort":"none","summary":"none"},"text":{"format":{"type":"text"}},"parallel_tool_calls":true});
        assert!(convert_request(request.clone(), Protocol::Responses, Protocol::Anthropic).is_ok());
        let chat =
            convert_request(request, Protocol::Responses, Protocol::ChatCompletions).unwrap();
        assert_eq!(chat["reasoning_effort"], "none");
    }
    #[test]
    fn chat_usage_trailer_reaches_completed_event() {
        let mut s = StreamConverter::new(Protocol::ChatCompletions, Protocol::Responses);
        s.push(
            "",
            &json!({"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]})
                .to_string(),
        )
        .unwrap();
        s.push(
            "",
            &json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}).to_string(),
        )
        .unwrap();
        s.push(
            "",
            &json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7}}).to_string(),
        )
        .unwrap();
        let done = s.push("", "[DONE]").unwrap().join("");
        assert!(done.contains("\"input_tokens\":11"));
        assert!(done.contains("response.completed"));
    }
    #[test]
    fn custom_tool_wrapping_and_streaming() {
        let request = json!({"input":[{"role":"user","content":"write a file"}],"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: \"*** Begin Patch\" body \"*** End Patch\""}}]});
        let ctx = custom_tools(&request).unwrap();
        let translated =
            convert_request(request, Protocol::Responses, Protocol::ChatCompletions).unwrap();
        assert_eq!(
            translated["tools"][0]["function"]["parameters"]["required"],
            json!(["input"])
        );
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch";
        let args = json!({"input":patch}).to_string();
        let response = json!({"id":"r","choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"apply_patch","arguments":args}}]},"finish_reason":"tool_calls"}]});
        let result = convert_response_with_tools(
            response,
            Protocol::ChatCompletions,
            Protocol::Responses,
            &ctx,
        )
        .unwrap();
        assert_eq!(result["output"][0]["type"], "custom_tool_call");
        assert_eq!(result["output"][0]["input"], patch);
        let mut stream = StreamConverter::new(Protocol::ChatCompletions, Protocol::Responses)
            .with_custom_tools(ctx);
        stream.push("",&json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"apply_patch","arguments":args}}]},"finish_reason":"tool_calls"}]}).to_string()).unwrap();
        let done = stream.push("", "[DONE]").unwrap().join("");
        assert!(done.contains("response.custom_tool_call_input.done"));
        assert!(done.contains("custom_tool_call"));
        assert!(custom_input("{\"input\":\"not a patch\"}", true).is_err());
    }
    #[test]
    fn custom_tool_history_and_unknown_grammar() {
        let request = json!({"input":[{"type":"custom_tool_call","call_id":"a","name":"run","input":"ls"},{"type":"custom_tool_call_output","call_id":"a","output":"a.txt"}]});
        let result = convert_request(request, Protocol::Responses, Protocol::Anthropic).unwrap();
        assert_eq!(result["messages"][0]["content"][0]["input"]["input"], "ls");
        assert_eq!(result["messages"][1]["content"][0]["tool_use_id"], "a");
        assert!(custom_tools(&json!({"tools":[{"type":"custom","name":"other","format":{"type":"grammar","syntax":"lark","definition":"start: WORD"}}]})).is_err());
    }
    #[test]
    fn client_tool_search_roundtrip() {
        let request = json!({"input":[{"role":"user","content":"find tools"}],"tools":[{"type":"tool_search","execution":"client","parameters":{"type":"object","properties":{"query":{"type":"string"}}}}]});
        let ctx = custom_tools(&request).unwrap();
        let chat =
            convert_request(request, Protocol::Responses, Protocol::ChatCompletions).unwrap();
        assert_eq!(chat["tools"][0]["function"]["name"], SEARCH_TOOL);
        let result=convert_response_with_tools(json!({"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"search1","type":"function","function":{"name":SEARCH_TOOL,"arguments":"{\"query\":\"files\"}"}}]},"finish_reason":"tool_calls"}]}),Protocol::ChatCompletions,Protocol::Responses,&ctx).unwrap();
        assert_eq!(result["output"][0]["type"], "tool_search_call");
        assert_eq!(result["output"][0]["arguments"]["query"], "files");
        let next = json!({"input":[result["output"][0],{"type":"tool_search_output","call_id":"search1","execution":"client","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}]});
        let next = convert_request(next, Protocol::Responses, Protocol::ChatCompletions).unwrap();
        assert_eq!(next["tools"][0]["function"]["name"], "read");
        assert_eq!(next["messages"][1]["tool_call_id"], "search1");
    }
    #[test]
    fn responses_parallel_call_history_is_one_chat_assistant_turn() {
        let request = json!({"input":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Reading two files"}]},{"type":"function_call","call_id":"a","name":"read","arguments":"{}"},{"type":"function_call","call_id":"b","name":"read","arguments":"{}"},{"type":"function_call_output","call_id":"a","output":"first"},{"type":"function_call_output","call_id":"b","output":"second"}]});
        let chat =
            convert_request(request, Protocol::Responses, Protocol::ChatCompletions).unwrap();
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(messages[0]["content"], "Reading two files");
        assert_eq!(messages[1]["tool_call_id"], "a");
        assert_eq!(messages[2]["tool_call_id"], "b");
    }
    #[test]
    fn anthropic_complete_and_empty_initial_tool_input() {
        for initial in [json!({}), json!({"path":"file.txt"})] {
            for to in [Protocol::ChatCompletions, Protocol::Responses] {
                let mut converter = StreamConverter::new(Protocol::Anthropic, to);
                let mut frames=converter.push("",&json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"read","input":initial}}).to_string()).unwrap();
                frames.extend(converter.push("",&json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}).to_string()).unwrap());
                converter.finish().unwrap();
                let values: Vec<Value> = frames
                    .iter()
                    .flat_map(|s| s.lines())
                    .filter_map(|s| s.strip_prefix("data: "))
                    .filter(|s| *s != "[DONE]")
                    .map(|s| serde_json::from_str(s).unwrap())
                    .collect();
                let arguments = if to == Protocol::Responses {
                    values.last().unwrap()["response"]["output"][0]["arguments"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                } else {
                    values
                        .iter()
                        .filter_map(|v| {
                            v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                                .as_str()
                        })
                        .collect::<String>()
                };
                assert_eq!(serde_json::from_str::<Value>(&arguments).unwrap(), initial);
            }
        }
    }
    fn stream_fixture(p: Protocol, length: bool) -> Vec<String> {
        let mut v = vec![];
        match p {
            Protocol::ChatCompletions => {
                for delta in [
                    json!({"content":"Hello"}),
                    json!({"tool_calls":[{"index":0,"id":"ca","function":{"name":"read","arguments":"{\"x\":"}},{"index":1,"id":"cb","function":{"name":"write","arguments":"{"}}]}),
                    json!({"tool_calls":[{"index":1,"function":{"arguments":"\"y\":2}"}},{"index":0,"function":{"arguments":"1}"}}]}),
                ] {
                    v.push(json!({"model":"test","choices":[{"index":0,"delta":delta,"finish_reason":null}]}));
                }
                v.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":if length{"length"}else{"tool_calls"}}]}));
                v.push(json!({"choices":[],"usage":{"prompt_tokens":13,"completion_tokens":9}}));
            }
            Protocol::Anthropic => {
                v.push(json!({"type":"message_start","message":{"model":"test","usage":{"input_tokens":13}}}));
                v.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                v.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}));
                v.push(json!({"type":"content_block_stop","index":0}));
                for (index, id, name, args) in [
                    (1, "ca", "read", "{\"x\":1}"),
                    (2, "cb", "write", "{\"y\":2}"),
                ] {
                    v.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
                    v.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":args}}));
                    v.push(json!({"type":"content_block_stop","index":index}));
                }
                v.push(json!({"type":"message_delta","delta":{"stop_reason":if length{"max_tokens"}else{"tool_use"}},"usage":{"output_tokens":9}}));
                v.push(json!({"type":"message_stop"}));
            }
            Protocol::Responses => {
                v.push(json!({"type":"response.created","response":{"model":"test"}}));
                v.push(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m"}}));
                v.push(json!({"type":"response.content_part.added","output_index":0,"part":{"type":"output_text"}}));
                v.push(
                    json!({"type":"response.output_text.delta","output_index":0,"delta":"Hello"}),
                );
                for (index, id, name, args) in [
                    (1, "ca", "read", "{\"x\":1}"),
                    (2, "cb", "write", "{\"y\":2}"),
                ] {
                    v.push(json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","call_id":id,"name":name,"arguments":""}}));
                    v.push(json!({"type":"response.function_call_arguments.delta","output_index":index,"delta":args}));
                }
                v.push(json!({"type":if length{"response.incomplete"}else{"response.completed"},"response":{"usage":{"input_tokens":13,"output_tokens":9}}}));
            }
        }
        let mut result: Vec<_> = v.into_iter().map(|v| v.to_string()).collect();
        if p == Protocol::ChatCompletions {
            result.push("[DONE]".into());
        }
        result
    }
    #[test]
    fn all_six_streaming_directions_preserve_parallel_tools_usage_and_limits() {
        for from in [
            Protocol::ChatCompletions,
            Protocol::Anthropic,
            Protocol::Responses,
        ] {
            for to in [
                Protocol::ChatCompletions,
                Protocol::Anthropic,
                Protocol::Responses,
            ] {
                if from == to {
                    continue;
                }
                for length in [false, true] {
                    let mut converter = StreamConverter::new(from, to);
                    let mut frames = vec![];
                    for frame in stream_fixture(from, length) {
                        frames.extend(converter.push("", &frame).unwrap());
                    }
                    frames.extend(converter.finish().unwrap());
                    let values: Vec<Value> = frames
                        .iter()
                        .flat_map(|s| s.lines())
                        .filter_map(|s| s.strip_prefix("data: "))
                        .filter(|s| *s != "[DONE]")
                        .map(|s| serde_json::from_str(s).unwrap())
                        .collect();
                    let wire = frames.join("");
                    assert!(wire.contains("Hello") && wire.contains("ca") && wire.contains("cb"));
                    match to {
                        Protocol::ChatCompletions => {
                            let reason = values
                                .iter()
                                .find_map(|v| v["choices"][0]["finish_reason"].as_str())
                                .unwrap();
                            assert_eq!(reason, if length { "length" } else { "tool_calls" });
                            let usage = values.iter().find(|v| !v["usage"].is_null()).unwrap();
                            assert_eq!(usage["usage"]["prompt_tokens"], 13);
                            assert_eq!(usage["usage"]["completion_tokens"], 9);
                            let mut args = std::collections::BTreeMap::<usize, String>::new();
                            for v in &values {
                                if let Some(calls) =
                                    v["choices"][0]["delta"]["tool_calls"].as_array()
                                {
                                    for c in calls {
                                        args.entry(c["index"].as_u64().unwrap() as usize)
                                            .or_default()
                                            .push_str(
                                                c["function"]["arguments"].as_str().unwrap_or(""),
                                            );
                                    }
                                }
                            }
                            assert_eq!(
                                serde_json::from_str::<Value>(&args[&0]).unwrap(),
                                json!({"x":1})
                            );
                            assert_eq!(
                                serde_json::from_str::<Value>(&args[&1]).unwrap(),
                                json!({"y":2})
                            );
                        }
                        Protocol::Anthropic => {
                            let mut open = None;
                            let mut count = 0;
                            for v in &values {
                                match v["type"].as_str() {
                                    Some("content_block_start") => {
                                        assert!(
                                            open.is_none(),
                                            "Anthropic blocks must be sequential"
                                        );
                                        open = v["index"].as_u64();
                                        count += 1;
                                    }
                                    Some("content_block_delta") => {
                                        assert_eq!(open, v["index"].as_u64())
                                    }
                                    Some("content_block_stop") => {
                                        assert_eq!(open, v["index"].as_u64());
                                        open = None;
                                    }
                                    _ => {}
                                }
                            }
                            assert!(open.is_none());
                            assert_eq!(count, 3);
                            let end = values
                                .iter()
                                .find(|v| v["type"] == "message_delta")
                                .unwrap();
                            assert_eq!(
                                end["delta"]["stop_reason"],
                                if length { "max_tokens" } else { "tool_use" }
                            );
                            assert_eq!(end["usage"]["output_tokens"], 9);
                            assert_eq!(end["usage"]["input_tokens"], 13);
                        }
                        Protocol::Responses => {
                            let end = values.last().unwrap();
                            assert_eq!(
                                end["type"],
                                if length {
                                    "response.incomplete"
                                } else {
                                    "response.completed"
                                }
                            );
                            assert_eq!(end["response"]["output"].as_array().unwrap().len(), 3);
                            assert_eq!(end["response"]["usage"]["input_tokens"], 13);
                            assert_eq!(end["response"]["usage"]["output_tokens"], 9);
                            assert_eq!(end["response"]["output"][1]["call_id"], "ca");
                            assert_eq!(end["response"]["output"][2]["call_id"], "cb");
                        }
                    }
                }
            }
        }
    }
}
