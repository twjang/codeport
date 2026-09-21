use crate::{
    classifier::{self, Review},
    config::{Auth, Backend, Protocol, SearchProvider},
    protocol,
};
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod integration_tests;

pub struct Bridge {
    pub base_url: String,
    pub token: String,
    task: tokio::task::JoinHandle<()>,
    state: Arc<BridgeState>,
}

#[derive(Clone)]
struct BridgeState {
    backend: Backend,
    search: SearchProvider,
    model: Option<String>,
    token: String,
    client: reqwest::Client,
    catalog: tokio::sync::OnceCell<Vec<Value>>,
}

impl Bridge {
    pub async fn model_ids(&self) -> Result<Vec<String>> {
        Ok(catalog(&self.state)
            .await?
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect())
    }

    #[cfg(test)]
    pub async fn start(backend: Backend, model: Option<String>) -> Result<Self> {
        Self::start_with_search(backend, model, SearchProvider::default()).await
    }

    pub async fn start_with_search(
        backend: Backend,
        model: Option<String>,
        search: SearchProvider,
    ) -> Result<Self> {
        search.validate()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let token = uuid::Uuid::new_v4().to_string();
        let state = Arc::new(BridgeState {
            catalog: tokio::sync::OnceCell::new(),
            backend,
            search,
            model,
            token: token.clone(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .build()?,
        });
        let app = Router::new()
            .route("/v1/chat/completions", post(handle))
            .route("/chat/completions", post(handle))
            .route("/v1/responses", post(handle))
            .route("/responses", post(handle))
            .route("/v1/messages", post(handle))
            .route("/messages", post(handle))
            .route("/v1/models", get(models))
            .route("/models", get(models))
            .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("codeport: local API bridge stopped: {error}");
            }
        });
        Ok(Self {
            base_url,
            token,
            task,
            state,
        })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {token}"))
        || headers.get("x-api-key").and_then(|h| h.to_str().ok()) == Some(token)
}

// Share one backend catalog snapshot between agent configuration and the bridge.
async fn catalog(state: &BridgeState) -> Result<&Vec<Value>> {
    state
        .catalog
        .get_or_try_init(|| async {
            if let Some(model) = &state.model {
                return Ok(vec![
                    json!({"id":model,"object":"model","created":0,"owned_by":"codeport"}),
                ]);
            }
            let mut url = endpoint(&state.backend.url, state.backend.protocol)?;
            let path = url.path().to_owned();
            let suffix = match state.backend.protocol {
                Protocol::ChatCompletions => "chat/completions",
                Protocol::Responses => "responses",
                Protocol::Anthropic => "messages",
            };
            url.set_path(&format!(
                "{}models",
                path.strip_suffix(suffix)
                    .context("invalid backend endpoint")?
            ));
            let mut request = state.client.get(url).timeout(Duration::from_secs(15));
            if state.backend.protocol == Protocol::Anthropic {
                request = request.header("anthropic-version", "2023-06-01");
            }
            request = match &state.backend.auth {
                Some(Auth::Bearer { token }) => request.bearer_auth(token),
                Some(Auth::Basic { username, password }) => {
                    request.basic_auth(username, Some(password))
                }
                None => request,
            };
            // Do not include upstream response bodies or URLs, which can contain secrets.
            let response = request
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("could not reach backend model endpoint"))?;
            if !response.status().is_success() {
                bail!("backend model endpoint returned HTTP {}", response.status());
            }
            let response: Value = response
                .json()
                .await
                .map_err(|_| anyhow::anyhow!("invalid JSON from backend model endpoint"))?;
            let entries = response["data"]
                .as_array()
                .context("backend model endpoint must return a data array")?;
            let mut seen = std::collections::HashSet::new();
            let mut models = Vec::new();
            for entry in entries {
                let id = entry["id"]
                    .as_str()
                    .filter(|id| !id.trim().is_empty())
                    .context("backend model catalog contains a missing or empty model ID")?;
                if seen.insert(id.to_owned()) {
                    models.push(entry.clone());
                }
            }
            if models.is_empty() {
                bail!("backend model endpoint returned no models");
            }
            Ok(models)
        })
        .await
        .context(
            "model discovery failed; check the backend /v1/models endpoint or set --model MODEL",
        )
}

async fn models(State(state): State<Arc<BridgeState>>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.token) {
        return error(StatusCode::UNAUTHORIZED, "invalid local bridge credential");
    }
    let models = match catalog(&state).await {
        Ok(models) => models,
        Err(err) => return error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
    };
    // Codex uses its own model catalog envelope; retain the standard OpenAI
    // list alongside it for clients that consume /v1/models as an OpenAI API.
    let codex_models: Vec<_> = models.iter().map(|entry| {
        let model = &entry["id"];
        json!({
        "slug": model,
        "display_name": model,
        "description": "Model configured through codeport",
        "default_reasoning_level": "none",
        "supported_reasoning_levels": [],
        "shell_type": "unified_exec",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 0,
        "base_instructions": "You are a coding assistant. Follow the user's instructions, inspect relevant files before editing, use available tools when needed, and verify your changes.",
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "supports_parallel_tool_calls": true,
        "apply_patch_tool_type": "freeform",
        "truncation_policy": {"mode":"tokens", "limit":10000},
        "context_window": 32768,
        "effective_context_window_percent": 95,
        "input_modalities": ["text"],
        "experimental_supported_tools": []
    })}).collect();
    Json(json!({"object":"list","data":models,"models":codex_models})).into_response()
}

async fn handle(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    uri: Uri,
    Json(mut body): Json<Value>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return error(StatusCode::UNAUTHORIZED, "invalid local bridge credential");
    }
    if !body.is_object() {
        return error(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
        );
    }
    let incoming = if uri.path().ends_with("/messages") {
        Protocol::Anthropic
    } else if uri.path().ends_with("/responses") {
        Protocol::Responses
    } else {
        Protocol::ChatCompletions
    };
    if let Some(model) = &state.model {
        body["model"] = json!(model);
    }
    if incoming == Protocol::Anthropic && incoming != state.backend.protocol {
        match crate::hosted_search::Search::parse(&body) {
            Ok(Some(search)) => return hosted_search_response(state, body, search).await,
            Ok(None) => {}
            Err(err) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    &format!("unsupported hosted search: {err}"),
                )
            }
        }
    }
    let review = if incoming == Protocol::Anthropic && incoming != state.backend.protocol {
        match Review::take(&mut body) {
            Ok(review) => review,
            Err(err) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    &format!("unsupported safeguards: {err}"),
                )
            }
        }
    } else {
        None
    };
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let custom = if incoming == Protocol::Responses && incoming != state.backend.protocol {
        match protocol::custom_tools(&body) {
            Ok(tools) => tools,
            Err(err) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    &format!("unsupported tool: {err:#}"),
                )
            }
        }
    } else {
        Default::default()
    };
    let converted = match protocol::convert_request(body, incoming, state.backend.protocol) {
        Ok(body) => body,
        Err(err) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("unsupported request: {err:#}"),
            )
        }
    };
    let endpoint = match endpoint(&state.backend.url, state.backend.protocol) {
        Ok(url) => url,
        Err(err) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("invalid backend URL: {err}"),
            )
        }
    };
    let mut request = state.client.post(endpoint).json(&converted);
    if state.backend.protocol == Protocol::Anthropic {
        request = request.header("anthropic-version", "2023-06-01");
        if incoming == Protocol::Anthropic {
            if let Some(value) = headers.get("anthropic-beta") {
                request = request.header("anthropic-beta", value);
            }
        }
    }
    request = match &state.backend.auth {
        Some(Auth::Bearer { token }) => request.bearer_auth(token),
        Some(Auth::Basic { username, password }) => request.basic_auth(username, Some(password)),
        None => request,
    };
    let upstream = match request.send().await {
        Ok(response) => response,
        Err(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "could not reach backend; check backend access, URL and TLS configuration",
            )
        }
    };
    if !upstream.status().is_success() {
        let status = upstream.status();
        // Do not echo upstream bodies that may contain credentials or reverse-proxy diagnostics.
        return error(status, &format!("backend returned HTTP {status}"));
    }
    if !stream {
        return match upstream.json::<Value>().await {
            Ok(body) => match protocol::convert_response_with_tools(
                body,
                state.backend.protocol,
                incoming,
                &custom,
            ) {
                Ok(mut body) => {
                    if let Some(review) = &review {
                        let tools = body["content"]
                            .as_array()
                            .unwrap_or(&Vec::new())
                            .iter()
                            .filter(|b| b["type"] == "tool_use")
                            .cloned()
                            .collect::<Vec<_>>();
                        body["safeguard_results"] = classify(&state, review, Ok(tools)).await;
                    }
                    Json(body).into_response()
                }
                Err(err) => error(
                    StatusCode::BAD_GATEWAY,
                    &format!("unsupported backend response: {err:#}"),
                ),
            },
            Err(_) => error(StatusCode::BAD_GATEWAY, "backend returned invalid JSON"),
        };
    }
    if !upstream
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.starts_with("text/event-stream"))
    {
        return error(
            StatusCode::BAD_GATEWAY,
            "backend did not return an SSE stream for a streaming request",
        );
    }
    if incoming == state.backend.protocol {
        return sse_response(Body::from_stream(upstream.bytes_stream()));
    }
    let from = state.backend.protocol;
    let output = async_stream::stream! {
        let mut converter = protocol::StreamConverter::new(from, incoming).with_custom_tools(custom);
        let mut chunks = upstream.bytes_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => { yield Ok::<Bytes, std::io::Error>(stream_error("backend stream interrupted")); return; }
            };
            buffer.extend_from_slice(&chunk);
            if buffer.len() > 32 * 1024 * 1024 {
                yield Ok(stream_error("backend SSE frame exceeds 32 MiB")); return;
            }
            while let Some((end, separator)) = frame_boundary(&buffer) {
                let frame = buffer.drain(..end + separator).collect::<Vec<_>>();
                let frame = match std::str::from_utf8(&frame[..end]) {
                    Ok(frame) => frame,
                    Err(_) => { yield Ok(stream_error("backend stream contains invalid UTF-8")); return; }
                };
                let (event, data) = parse_frame(frame);
                if data.is_empty() { continue; }
                match converter.push(&event, &data) {
                    Ok(frames) => for frame in frames {
                        let pending = reviewed_frame(&state, review.as_ref(), &converter, frame);
                        tokio::pin!(pending);
                        let frame = loop {
                            tokio::select! {
                                frame = &mut pending => break frame,
                                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                                    yield Ok(Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"));
                                }
                            }
                        };
                        yield Ok(Bytes::from(frame));
                    },
                    Err(err) => { yield Ok(stream_error(&format!("unsupported backend stream: {err:#}"))); return; }
                }
            }
        }
        match converter.finish() {
            Ok(frames) => for frame in frames {
                        let pending = reviewed_frame(&state, review.as_ref(), &converter, frame);
                        tokio::pin!(pending);
                        let frame = loop {
                            tokio::select! {
                                frame = &mut pending => break frame,
                                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                                    yield Ok(Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"));
                                }
                            }
                        };
                        yield Ok(Bytes::from(frame));
                    },
            Err(err) => yield Ok(stream_error(&format!("incomplete backend stream: {err:#}"))),
        }
    };
    sse_response(Body::from_stream(output))
}

async fn hosted_search_response(
    state: Arc<BridgeState>,
    body: Value,
    search: crate::hosted_search::Search,
) -> Response {
    use crate::hosted_search::{block_events, event};
    let id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let tool_id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
    let tool = search.tool_block(&tool_id);
    let message = json!({"id":id,"type":"message","role":"assistant","model":body["model"],"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}});
    if body["stream"] != true {
        let (mut blocks, uses) = search.results(&state.search, &tool_id).await;
        blocks.insert(0, tool);
        let mut message = message;
        message["content"] = json!(blocks);
        message["stop_reason"] = json!("end_turn");
        message["usage"]["server_tool_use"] = json!({"web_search_requests":uses});
        return Json(message).into_response();
    }
    let output = async_stream::stream! {
        yield Ok::<Bytes,std::io::Error>(Bytes::from(event(json!({"type":"message_start","message":message}))));
        for frame in block_events(0,&tool) { yield Ok(Bytes::from(frame)); }
        let pending = search.results(&state.search,&tool_id);
        tokio::pin!(pending);
        let (blocks,uses) = loop {
            tokio::select! {
                result = &mut pending => break result,
                _ = tokio::time::sleep(Duration::from_secs(10)) => yield Ok(Bytes::from(event(json!({"type":"ping"})))),
            }
        };
        for (index,block) in blocks.iter().enumerate() {
            for frame in block_events(index+1,block) { yield Ok(Bytes::from(frame)); }
        }
        yield Ok(Bytes::from(event(json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":0,"server_tool_use":{"web_search_requests":uses}}}))));
        yield Ok(Bytes::from(event(json!({"type":"message_stop"}))));
    };
    sse_response(Body::from_stream(output))
}

async fn classify(state: &BridgeState, review: &Review, tools: Result<Vec<Value>>) -> Value {
    let (pending, allowed) = match tools.and_then(classifier::allow_web_tools) {
        Ok(parts) => parts,
        Err(_) => return classifier::unavailable("error"),
    };
    let result = review_tools(state, review, &pending).await;
    classifier::merge_web_results(result, allowed, &pending)
}

async fn review_tools(state: &BridgeState, review: &Review, tools: &[Value]) -> Value {
    if tools.is_empty() {
        return classifier::available(json!({}));
    }
    let operation = async {
        let body = protocol::convert_request(
            review.request(tools)?,
            Protocol::Anthropic,
            state.backend.protocol,
        )?;
        let mut request = state
            .client
            .post(endpoint(&state.backend.url, state.backend.protocol)?)
            .json(&body);
        request = match &state.backend.auth {
            Some(Auth::Bearer { token }) => request.bearer_auth(token),
            Some(Auth::Basic { username, password }) => {
                request.basic_auth(username, Some(password))
            }
            None => request,
        };
        let response = request.send().await?.error_for_status()?;
        let reply = response.json::<Value>().await?;
        let reply = protocol::convert_response(reply, state.backend.protocol, Protocol::Anthropic)?;
        classifier::results(&reply, tools)
    };
    match tokio::time::timeout(Duration::from_secs(60), operation).await {
        Ok(Ok(results)) => results,
        Ok(Err(_)) => {
            eprintln!("codeport: tool review failed; proposed actions remain blocked");
            classifier::unavailable("error")
        }
        Err(_) => classifier::unavailable("timeout"),
    }
}

async fn reviewed_frame(
    state: &BridgeState,
    review: Option<&Review>,
    converter: &protocol::StreamConverter,
    frame: String,
) -> String {
    let Some(review) = review else { return frame };
    let (event, data) = parse_frame(&frame);
    if event != "message_delta" {
        return frame;
    }
    // The converter creates valid JSON. Delay completion until every tool has a verdict.
    let mut value: Value = serde_json::from_str(&data).expect("generated SSE JSON");
    value["delta"]["safeguard_results"] = classify(state, review, converter.tool_uses()).await;
    format!("event: message_delta\ndata: {value}\n\n")
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap()
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"type":"error","error":{"type":"codeport_error","message":message}})),
    )
        .into_response()
}

fn stream_error(message: &str) -> Bytes {
    Bytes::from(format!(
        "event: error\ndata: {}\n\n",
        json!({"type":"error","error":{"type":"codeport_error","message":message}})
    ))
}

fn endpoint(base: &str, protocol: Protocol) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base)?;
    let suffix = match protocol {
        Protocol::Anthropic => "messages",
        Protocol::Responses => "responses",
        Protocol::ChatCompletions => "chat/completions",
    };
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with(&format!("/{suffix}")) {
        path.to_owned()
    } else if path.is_empty() {
        format!("/v1/{suffix}")
    } else {
        format!("{path}/{suffix}")
    };
    url.set_path(&path);
    if url.host_str().is_none() {
        anyhow::bail!("missing hostname");
    }
    Ok(url)
}

fn frame_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    (0..bytes.len()).find_map(|i| {
        if bytes[i..].starts_with(b"\n\n") {
            Some((i, 2))
        } else if bytes[i..].starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else {
            None
        }
    })
}

fn parse_frame(frame: &str) -> (String, String) {
    let mut event = String::new();
    let mut data = Vec::new();
    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = value.strip_prefix(' ').unwrap_or(value).to_owned();
        }
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    (event, data.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_paths() {
        assert_eq!(
            endpoint("http://localhost:8000", Protocol::Responses)
                .unwrap()
                .as_str(),
            "http://localhost:8000/v1/responses"
        );
        assert_eq!(
            endpoint("https://example.com/api/v1/", Protocol::Anthropic)
                .unwrap()
                .as_str(),
            "https://example.com/api/v1/messages"
        );
        assert_eq!(
            endpoint(
                "https://example.com/v1/chat/completions",
                Protocol::ChatCompletions
            )
            .unwrap()
            .as_str(),
            "https://example.com/v1/chat/completions"
        );
    }
    #[test]
    fn sse_framing() {
        assert_eq!(frame_boundary(b"data: x\r\n\r\ndata:"), Some((7, 4)));
        assert_eq!(
            parse_frame("event: delta\r\ndata: one\r\ndata: two"),
            ("delta".into(), "one\ntwo".into())
        );
    }
}
