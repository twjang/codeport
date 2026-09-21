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

#[tokio::test]
async fn safeguards_review_streamed_and_nonstreamed_tools_and_fail_closed() {
    async fn backend(
        State((requests, verdict)): State<(Requests, String)>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        requests.lock().unwrap().push((headers, body.clone()));
        if body.get("tools").is_none() {
            if verdict == "http_error" {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            let content = if verdict == "malformed" {
                "not json".to_owned()
            } else {
                json!({"decisions":[{"tool_use_id":"call_review","outcome":verdict,"explanation":"Reviewed user scope"}]}).to_string()
            };
            return Json(json!({"id":"review","model":"local","choices":[{"message":{"role":"assistant","content":content},"finish_reason":"stop"}]})).into_response();
        }
        let call = json!({"id":"call_review","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"pwd\"}"}});
        if body["stream"] == true {
            let mut call = call;
            call["index"] = json!(0);
            let first = json!({"id":"generation","model":"local","choices":[{"index":0,"delta":{"tool_calls":[call]},"finish_reason":null}]});
            let last = json!({"id":"generation","model":"local","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
            sse_response(Body::from(format!(
                "data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"
            )))
        } else {
            Json(json!({"id":"generation","model":"local","choices":[{"message":{"role":"assistant","content":null,"tool_calls":[call]},"finish_reason":"tool_calls"}]})).into_response()
        }
    }
    for streamed in [false, true] {
        for verdict in ["not_flagged", "flagged", "malformed", "http_error"] {
            let requests: Requests = Arc::default();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .fallback(post(backend))
                .with_state((requests.clone(), verdict.to_owned()));
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let bridge = Bridge::start(
                Backend {
                    url,
                    protocol: Protocol::ChatCompletions,
                    model: None,
                    auth: Some(Auth::Bearer {
                        token: "review-secret".into(),
                    }),
                    access: None,
                },
                Some("local".into()),
            )
            .await
            .unwrap();
            let response = reqwest::Client::new().post(format!("{}/v1/messages", bridge.base_url)).bearer_auth(&bridge.token).json(&json!({
                "model":"client","stream":streamed,"max_tokens":100,
                "messages":[{"role":"user","content":"Print the working directory"}],
                "tools":[{"name":"Bash","input_schema":{"type":"object"}}],
                "safeguards":[{"type":"dangerous_tool_use","classifier_context":{"v":1,"rules":{},"auto_mode":{},"trusted_directories":{}}}]
            })).send().await.unwrap();
            assert!(response.status().is_success());
            let text = response.text().await.unwrap();
            let result = if streamed {
                assert!(
                    text.find("safeguard_results").unwrap()
                        < text.find("event: message_stop").unwrap()
                );
                let frame = text
                    .split("\n\n")
                    .find(|frame| frame.contains("safeguard_results"))
                    .unwrap();
                let (_, data) = parse_frame(frame);
                serde_json::from_str::<Value>(&data).unwrap()["delta"]["safeguard_results"].clone()
            } else {
                serde_json::from_str::<Value>(&text).unwrap()["safeguard_results"].clone()
            };
            let status = &result[0]["status"];
            if verdict == "malformed" || verdict == "http_error" {
                assert_eq!(status["type"], "unavailable");
                assert_eq!(status["reason"], "error");
            } else {
                assert_eq!(status["tool_uses"]["call_review"]["outcome"], verdict);
            }
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let (headers, review) = &requests[1];
            assert_eq!(headers["authorization"], "Bearer review-secret");
            assert_eq!(review["model"], "local");
            assert!(review.get("tools").is_none());
            assert_eq!(review["reasoning_effort"], "none");
            assert!(review.to_string().contains("Print the working directory"));
            assert!(requests[0].1.get("safeguards").is_none());
            task.abort();
        }
    }
}

#[tokio::test]
async fn safeguards_skip_empty_tool_batches_and_preserve_native_reviews() {
    for native in [false, true] {
        let requests: Requests = Arc::default();
        let expected =
            json!([{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{}}}]);
        let reply = if native {
            json!({"type":"message","role":"assistant","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","safeguard_results":expected})
        } else {
            json!({"id":"reply","model":"local","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]})
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .fallback(post(mock))
            .with_state((requests.clone(), reply));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bridge = Bridge::start(
            Backend {
                url,
                protocol: if native {
                    Protocol::Anthropic
                } else {
                    Protocol::ChatCompletions
                },
                model: None,
                auth: None,
                access: None,
            },
            Some("local".into()),
        )
        .await
        .unwrap();
        let safeguards = json!([{"type":"dangerous_tool_use","classifier_context":{"v":1,"rules":{},"auto_mode":{},"trusted_directories":{}}}]);
        let response: Value = reqwest::Client::new().post(format!("{}/v1/messages", bridge.base_url)).bearer_auth(&bridge.token).json(&json!({"model":"local","messages":[{"role":"user","content":"Hello"}],"max_tokens":100,"safeguards":safeguards})).send().await.unwrap().json().await.unwrap();
        assert_eq!(response["safeguard_results"], expected);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        if native {
            assert_eq!(requests[0].1["safeguards"], safeguards);
        }
        task.abort();
    }
}

#[tokio::test]
async fn native_websearch_returns_streamed_and_json_results_with_limits() {
    async fn provider(
        axum::extract::Query(query): axum::extract::Query<
            std::collections::HashMap<String, String>,
        >,
    ) -> Response {
        if query["q"] == "failure" {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Json(json!({"results":[{"title":"Allowed","url":"https://docs.example.com/guide","content":"A useful snippet"},{"title":"Excluded","url":"https://unrelated.example/","content":"Must not leak through the filter"}]})).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/search", get(provider)))
            .await
            .unwrap()
    });
    let bridge = Bridge::start_with_search(
        Backend {
            url: "http://127.0.0.1:1".into(),
            protocol: Protocol::ChatCompletions,
            model: None,
            auth: None,
            access: None,
        },
        Some("local".into()),
        SearchProvider::Searxng { url },
    )
    .await
    .unwrap();
    for stream in [false, true] {
        for scenario in ["success", "budget", "failure", "blocked"] {
            let mut tool = json!({"type":"web_search_20250305","name":"web_search","max_uses":8});
            if scenario == "blocked" {
                tool["blocked_domains"] = json!(["unrelated.example"]);
            } else {
                tool["allowed_domains"] = json!(["example.com"]);
            }
            if scenario == "budget" {
                tool["max_uses"] = json!(0);
            }
            let query = if scenario == "failure" {
                "failure"
            } else {
                "Rust docs"
            };
            let body = json!({"model":"client","messages":[{"role":"user","content":[{"type":"text","text":format!("Perform a web search for the query: {query}")}]}],"tools":[tool],"max_tokens":32000,"stream":stream});
            let response = reqwest::Client::new()
                .post(format!("{}/v1/messages", bridge.base_url))
                .bearer_auth(&bridge.token)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            let text = response.text().await.unwrap();
            let blocks: Vec<Value> = if stream {
                assert!(text.contains("event: message_stop"));
                text.split("\n\n")
                    .filter_map(|frame| {
                        let (kind, data) = parse_frame(frame);
                        if kind == "content_block_start" {
                            Some(
                                serde_json::from_str::<Value>(&data).unwrap()["content_block"]
                                    .clone(),
                            )
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                serde_json::from_str::<Value>(&text).unwrap()["content"]
                    .as_array()
                    .unwrap()
                    .clone()
            };
            assert_eq!(blocks[0]["type"], "server_tool_use");
            assert_eq!(blocks[0]["name"], "web_search");
            assert_eq!(blocks[1]["type"], "web_search_tool_result");
            assert_eq!(blocks[1]["tool_use_id"], blocks[0]["id"]);
            if matches!(scenario, "success" | "blocked") {
                assert_eq!(blocks[1]["content"].as_array().unwrap().len(), 1);
                assert_eq!(
                    blocks[1]["content"][0]["url"],
                    "https://docs.example.com/guide"
                );
                assert!(text.contains("A useful snippet"));
                assert!(!text.contains("Must not leak"));
            } else {
                assert_eq!(
                    blocks[1]["content"]["error_code"],
                    if scenario == "budget" {
                        "max_uses_exceeded"
                    } else {
                        "unavailable"
                    }
                );
            }
        }
    }
    task.abort();
}

#[tokio::test]
async fn classifier_allows_web_tools_without_reviewing_them() {
    let requests: Requests = Arc::default();
    async fn reviewer(
        State(requests): State<Requests>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        requests.lock().unwrap().push((headers, body.clone()));
        // Verify the review contains only Bash as a proposed action.
        let evidence: Value = serde_json::from_str(
            body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(evidence["proposed_tool_uses"].as_array().unwrap().len(), 1);
        assert_eq!(evidence["proposed_tool_uses"][0]["name"], "Bash");
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(post(reviewer))
        .with_state(requests.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = BridgeState {
        catalog: tokio::sync::OnceCell::new(),
        backend: Backend {
            url,
            protocol: Protocol::ChatCompletions,
            model: None,
            auth: None,
            access: None,
        },
        search: SearchProvider::Public,
        model: Some("local".into()),
        token: "local-token".into(),
        client: reqwest::Client::new(),
    };
    let mut body = json!({"model":"local","messages":[{"role":"user","content":"Search and fetch pages"}],"safeguards":[{"type":"dangerous_tool_use","classifier_context":{"v":1,"rules":{},"auto_mode":{},"trusted_directories":{}}}]});
    let review = Review::take(&mut body).unwrap().unwrap();
    let web = vec![
        json!({"type":"tool_use","id":"fetch","name":"WebFetch","input":{"url":"https://example.com"}}),
        json!({"type":"tool_use","id":"search","name":"WebSearch","input":{"query":"docs"}}),
    ];
    let result = classify(&state, &review, Ok(web.clone())).await;
    assert_eq!(
        result[0]["status"]["tool_uses"]["fetch"]["outcome"],
        "not_flagged"
    );
    assert_eq!(
        result[0]["status"]["tool_uses"]["search"]["outcome"],
        "not_flagged"
    );
    assert!(requests.lock().unwrap().is_empty());
    let mut mixed = web;
    mixed.push(json!({"type":"tool_use","id":"shell","name":"Bash","input":{"command":"pwd"}}));
    let result = classify(&state, &review, Ok(mixed)).await;
    assert_eq!(
        result[0]["status"]["tool_uses"]["fetch"]["outcome"],
        "not_flagged"
    );
    assert_eq!(
        result[0]["status"]["tool_uses"]["search"]["outcome"],
        "not_flagged"
    );
    assert_eq!(
        result[0]["status"]["tool_uses"]["shell"]["type"],
        "unavailable"
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn discovers_backend_models_with_auth_and_preserves_selection() {
    for (base_path, auth, expected_auth) in [
        ("", None, None),
        (
            "/prefix/v1/",
            Some(Auth::Bearer {
                token: "upstream-secret".into(),
            }),
            Some("Bearer upstream-secret"),
        ),
        (
            "/prefix/v1/chat/completions",
            Some(Auth::Basic {
                username: "user".into(),
                password: "pass".into(),
            }),
            Some("Basic dXNlcjpwYXNz"),
        ),
    ] {
        let requests: Requests = Arc::default();
        let captured = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let prefix = if base_path.is_empty() {
            "/v1"
        } else {
            "/prefix/v1"
        };
        let app = Router::new()
            .route(
                &format!("{prefix}/models"),
                get(move |headers: HeaderMap| async move {
                    captured.lock().unwrap().push((headers, Value::Null));
                    Json(json!({"object":"list","data":[
                        {"id":"local/first","object":"model","max_model_len":8192},
                        {"id":"local/second","object":"model"},
                        {"id":"local/first"}
                    ]}))
                }),
            )
            .route(&format!("{prefix}/chat/completions"), post(mock))
            .with_state((requests.clone(), json!({"choices":[]})));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bridge = Bridge::start(
            Backend {
                url: format!("{origin}{base_path}"),
                protocol: Protocol::ChatCompletions,
                model: None,
                auth,
                access: None,
            },
            None,
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();
        assert_eq!(
            client
                .get(format!("{}/models", bridge.base_url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(requests.lock().unwrap().is_empty());
        assert_eq!(
            bridge.model_ids().await.unwrap(),
            ["local/first", "local/second"]
        );
        for path in ["/v1/models", "/models"] {
            let list: Value = client
                .get(format!("{}{path}", bridge.base_url))
                .bearer_auth(&bridge.token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(list["data"].as_array().unwrap().len(), 2);
            assert_eq!(list["data"][0]["max_model_len"], 8192);
            assert_eq!(list["models"][1]["slug"], "local/second");
        }
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "catalog should be cached"
        );
        let response = client
            .post(format!("{}/v1/chat/completions", bridge.base_url))
            .bearer_auth(&bridge.token)
            .json(&json!({"model":"local/second","messages":[]}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let captured = requests.lock().unwrap();
        assert_eq!(
            captured[1].1["model"], "local/second",
            "discovery must not pin the first model"
        );
        for (headers, _) in captured.iter() {
            assert_eq!(
                headers.get("authorization").map(|h| h.to_str().unwrap()),
                expected_auth
            );
        }
        task.abort();
    }
}

#[tokio::test]
async fn discovery_reports_invalid_empty_and_failed_catalogs() {
    for (status, body) in [
        (StatusCode::OK, r#"{"data":[]}"#),
        (StatusCode::OK, r#"{"models":[]}"#),
        (StatusCode::OK, r#"{"data":[{"id":" "}]}"#),
        (StatusCode::OK, r#"{"data":[{"id":123}]}"#),
        (StatusCode::OK, "invalid JSON"),
        (StatusCode::UNAUTHORIZED, "private error details"),
        (StatusCode::NOT_FOUND, "private error details"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/models", get(move || async move { (status, body) }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
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
        let response = reqwest::Client::new()
            .get(format!("{}/v1/models", bridge.base_url))
            .bearer_auth(&bridge.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let text = response.text().await.unwrap();
        assert!(text.contains("--model MODEL"), "{text}");
        assert!(!text.contains("private error details"));
        task.abort();
    }
}
