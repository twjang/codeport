use super::*;
use axum::extract::State;
use std::sync::Mutex;

type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

async fn mock(
    State((requests, response)): State<(Requests, Value)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    requests.lock().unwrap().push((headers, body));
    Json(response)
}

#[tokio::test]
async fn authenticated_protocol_matrix_and_tool_results() {
    let protocols = [
        Protocol::ChatCompletions,
        Protocol::Responses,
        Protocol::Anthropic,
    ];
    for upstream_protocol in protocols {
        let response = match upstream_protocol {
            Protocol::ChatCompletions => {
                json!({"id":"reply","model":"backend","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}})
            }
            Protocol::Responses => {
                json!({"id":"reply","model":"backend","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":3,"output_tokens":1}})
            }
            Protocol::Anthropic => {
                json!({"id":"reply","model":"backend","type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}})
            }
        };
        let requests: Requests = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .fallback(post(mock))
            .with_state((requests.clone(), response));
        let mock_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bridge = Bridge::start(
            Backend {
                url,
                protocol: upstream_protocol,
                model: None,
                auth: Some(Auth::Bearer {
                    token: "upstream-secret".into(),
                }),
                access: None,
            },
            Some("forced-model".into()),
        )
        .await
        .unwrap();
        for agent_protocol in protocols {
            let request = json!({"model":"agent-model","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]},{"role":"tool","tool_call_id":"call_1","content":"file contents"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]});
            let request =
                protocol::convert_request(request, Protocol::ChatCompletions, agent_protocol)
                    .unwrap();
            let endpoint = endpoint(&bridge.base_url, agent_protocol).unwrap();
            let result = reqwest::Client::new()
                .post(endpoint)
                .bearer_auth(&bridge.token)
                .json(&request)
                .send()
                .await
                .unwrap();
            assert!(
                result.status().is_success(),
                "{agent_protocol:?} -> {upstream_protocol:?}: {}",
                result.text().await.unwrap()
            );
            let response: Value = result.json().await.unwrap();
            assert!(response.to_string().contains("done"), "{response}");
            let requests = requests.lock().unwrap();
            let (headers, request) = requests.last().unwrap();
            assert_eq!(headers["authorization"], "Bearer upstream-secret");
            assert_eq!(request["model"], "forced-model");
            assert!(request.to_string().contains("file contents"));
            assert!(request.to_string().contains("call_1"));
        }
        assert_eq!(requests.lock().unwrap().len(), 3);
        mock_task.abort();
    }
}

#[tokio::test]
async fn local_auth_blocks_unauthenticated_requests() {
    let bridge = Bridge::start(
        Backend {
            url: "http://127.0.0.1:1".into(),
            protocol: Protocol::Responses,
            model: None,
            auth: None,
            access: None,
        },
        None,
    )
    .await
    .unwrap();
    let result = reqwest::Client::new()
        .post(format!("{}/v1/responses", bridge.base_url))
        .json(&json!({"input":"hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_object_request_returns_bad_request_instead_of_panicking() {
    let bridge = Bridge::start(
        Backend {
            url: "http://127.0.0.1:1".into(),
            protocol: Protocol::Responses,
            model: None,
            auth: None,
            access: None,
        },
        Some("override".into()),
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    for body in [json!([]), json!("hello"), Value::Null] {
        let result = client
            .post(format!("{}/v1/responses", bridge.base_url))
            .bearer_auth(&bridge.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn forwards_basic_auth_and_preserves_empty_model() {
    let requests: Requests = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(post(mock))
        .with_state((requests.clone(), json!({"choices":[]})));
    let mock_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bridge = Bridge::start(
        Backend {
            url,
            protocol: Protocol::ChatCompletions,
            model: None,
            auth: Some(Auth::Basic {
                username: "user".into(),
                password: "pass".into(),
            }),
            access: None,
        },
        None,
    )
    .await
    .unwrap();
    let result = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", bridge.base_url))
        .bearer_auth(&bridge.token)
        .json(&json!({"model":"agent-choice","messages":[]}))
        .send()
        .await
        .unwrap();
    assert!(result.status().is_success());
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].0["authorization"], "Basic dXNlcjpwYXNz");
    assert_eq!(requests[0].1["model"], "agent-choice");
    mock_task.abort();
}

#[tokio::test]
async fn fragmented_sse_is_translated_and_finishes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let frames = [
        json!({"id":"chat_1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"한글"},"finish_reason":null}]}),
        json!({"id":"chat_1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ].iter().map(|v| format!("data: {v}\r\n\r\n")).collect::<String>() + "data: [DONE]\r\n\r\n";
    let app = Router::new().fallback(post(move || async move {
        let chunks = frames
            .into_bytes()
            .into_iter()
            .map(|byte| Ok::<_, std::io::Error>(Bytes::from(vec![byte])));
        sse_response(Body::from_stream(futures_util::stream::iter(chunks)))
    }));
    let mock_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bridge = Bridge::start(
        Backend {
            url,
            protocol: Protocol::ChatCompletions,
            model: None,
            auth: None,
            access: None,
        },
        None,
    )
    .await
    .unwrap();
    let result = reqwest::Client::new()
        .post(format!("{}/v1/responses", bridge.base_url))
        .bearer_auth(&bridge.token)
        .json(&json!({"model":"m","input":"hello","stream":true}))
        .send()
        .await
        .unwrap();
    assert!(result.status().is_success());
    let text = result.text().await.unwrap();
    assert!(text.contains("response.output_text.delta"), "{text}");
    assert!(text.contains("한글"), "{text}");
    assert!(text.contains("response.completed"), "{text}");
    assert!(!text.contains("event: error"), "{text}");
    mock_task.abort();
}

#[tokio::test]
async fn model_catalog_supports_codex_and_openai_clients() {
    let bridge = Bridge::start(
        Backend {
            url: "http://127.0.0.1:1".into(),
            protocol: Protocol::ChatCompletions,
            model: None,
            auth: None,
            access: None,
        },
        Some("local/qwen".into()),
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let url = format!("{}/v1/models?client_version=0.155.1", bridge.base_url);
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let catalog: Value = client
        .get(&url)
        .bearer_auth(&bridge.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(catalog["data"][0]["id"], "local/qwen");
    assert_eq!(catalog["models"][0]["slug"], "local/qwen");
    assert_eq!(catalog["models"][0]["input_modalities"], json!(["text"]));
    assert!(catalog["models"][0]["experimental_supported_tools"].is_array());
}
