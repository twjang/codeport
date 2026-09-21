//! Compatibility for Claude Code's single-query WebSearch auxiliary request.
//! Native Anthropic backends retain their own hosted search implementation.
use crate::{config::SearchProvider, web_search};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

pub struct Search {
    pub query: String,
    allowed: Vec<String>,
    blocked: Vec<String>,
    max_uses: u64,
}

impl Search {
    pub fn parse(body: &Value) -> Result<Option<Self>> {
        let Some(tools) = body["tools"].as_array() else {
            return Ok(None);
        };
        if !tools.iter().any(|tool| {
            tool["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("web_search_"))
        }) {
            return Ok(None);
        }
        ensure!(
            tools.len() == 1
                && tools[0]["type"] == "web_search_20250305"
                && tools[0]["name"] == "web_search",
            "only Claude Code's standalone web_search_20250305 request is supported"
        );
        let tool = &tools[0];
        for key in tool.as_object().unwrap().keys() {
            ensure!(
                matches!(
                    key.as_str(),
                    "type"
                        | "name"
                        | "max_uses"
                        | "allowed_domains"
                        | "blocked_domains"
                        | "cache_control"
                ),
                "unsupported web search option: {key}"
            );
        }
        let max_uses = match tool.get("max_uses") {
            None => 1,
            Some(value) => value
                .as_u64()
                .context("web search max_uses must be a nonnegative integer")?,
        };
        let allowed = domains(tool.get("allowed_domains"))?;
        let blocked = domains(tool.get("blocked_domains"))?;
        ensure!(
            tool.get("allowed_domains").is_none() || tool.get("blocked_domains").is_none(),
            "use allowed_domains or blocked_domains, not both"
        );
        let choice = &body["tool_choice"];
        ensure!(
            choice.is_null()
                || choice["type"] == "auto"
                || choice["type"] == "any"
                || (choice["type"] == "tool" && choice["name"] == "web_search"),
            "unsupported web search tool_choice"
        );
        ensure!(body.get("safeguards").is_none(), "standalone web search does not support safeguards; Claude applies permissions to the outer WebSearch call");
        let messages = body["messages"]
            .as_array()
            .context("web search requires messages")?;
        ensure!(
            messages.len() == 1 && messages[0]["role"] == "user",
            "expected Claude Code's single-query WebSearch request"
        );
        let text = if let Some(text) = messages[0]["content"].as_str() {
            text
        } else {
            let blocks = messages[0]["content"]
                .as_array()
                .context("invalid search query content")?;
            ensure!(
                blocks.len() == 1 && blocks[0]["type"] == "text",
                "expected one search query text block"
            );
            blocks[0]["text"]
                .as_str()
                .context("invalid search query text")?
        };
        let query = text.strip_prefix("Perform a web search for the query: ").context("expected Claude Code's explicit WebSearch query; general hosted-search conversations are unsupported")?;
        ensure!(!query.trim().is_empty(), "search query is empty");
        Ok(Some(Self {
            query: query.to_owned(),
            allowed,
            blocked,
            max_uses,
        }))
    }

    pub fn tool_block(&self, id: &str) -> Value {
        json!({"type":"server_tool_use","id":id,"name":"web_search","input":{"query":self.query}})
    }

    pub async fn results(&self, provider: &SearchProvider, id: &str) -> (Vec<Value>, u64) {
        if self.max_uses == 0 {
            return (vec![failure(id, "max_uses_exceeded")], 0);
        }
        if self.query.len() > 2000 {
            return (vec![failure(id, "query_too_long")], 0);
        }
        let result = web_search::search(provider, &self.query, 10).await;
        match result {
            Ok(result) => {
                let results: Vec<_> = result
                    .iter()
                    .filter(|item| {
                        let url = item["url"].as_str().unwrap_or("");
                        (self.allowed.is_empty()
                            || self
                                .allowed
                                .iter()
                                .any(|domain| domain_matches(url, domain)))
                            && !self
                                .blocked
                                .iter()
                                .any(|domain| domain_matches(url, domain))
                    })
                    .collect();
                let links: Vec<_> = results.iter().map(|item| json!({"type":"web_search_result","title":item["title"],"url":item["url"]})).collect();
                let text = if results.is_empty() {
                    "No matching search results were returned.".to_owned()
                } else {
                    results
                        .iter()
                        .map(|item| {
                            format!(
                                "{}\n{}\n{}",
                                item["title"].as_str().unwrap_or(""),
                                item["url"].as_str().unwrap_or(""),
                                item["snippet"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n")
                };
                (
                    vec![
                        json!({"type":"web_search_tool_result","tool_use_id":id,"content":links}),
                        json!({"type":"text","text":text}),
                    ],
                    1,
                )
            }
            Err(error) => (
                vec![
                    failure(id, "unavailable"),
                    json!({"type":"text","text":format!("Search provider error: {error}")}),
                ],
                1,
            ),
        }
    }
}

fn failure(id: &str, code: &str) -> Value {
    json!({"type":"web_search_tool_result","tool_use_id":id,"content":{"type":"web_search_tool_result_error","error_code":code}})
}

fn domains(value: Option<&Value>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value.as_array().context("domain filters must be arrays")?;
    values
        .iter()
        .map(|value| {
            let domain = value
                .as_str()
                .context("domain filters must contain strings")?;
            ensure!(
                !domain.is_empty() && !domain.contains("://") && !domain.contains('*'),
                "domain filters require bare domains with optional paths"
            );
            let url = reqwest::Url::parse(&format!("https://{domain}"))?;
            ensure!(
                url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.port().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "invalid domain filter"
            );
            Ok(format!(
                "{}{}",
                url.host_str().unwrap(),
                url.path().trim_end_matches('/')
            ))
        })
        .collect()
}

fn domain_matches(raw: &str, domain: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    let (host, path) = domain
        .split_once('/')
        .map(|(host, path)| (host, format!("/{path}")))
        .unwrap_or((domain, String::new()));
    let actual = url.host_str().unwrap_or("");
    (actual == host || actual.ends_with(&format!(".{host}")))
        && (path.is_empty() || url.path() == path || url.path().starts_with(&format!("{path}/")))
}

pub fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
pub fn block_events(index: usize, block: &Value) -> Vec<String> {
    let mut events = Vec::new();
    let mut start = block.clone();
    let delta = match block["type"].as_str() {
        Some("text") => {
            start["text"] = json!("");
            Some(json!({"type":"text_delta","text":block["text"]}))
        }
        Some("server_tool_use") => {
            start["input"] = json!({});
            Some(json!({"type":"input_json_delta","partial_json":block["input"].to_string()}))
        }
        _ => None,
    };
    events.push(event(
        json!({"type":"content_block_start","index":index,"content_block":start}),
    ));
    if let Some(delta) = delta {
        events.push(event(
            json!({"type":"content_block_delta","index":index,"delta":delta}),
        ));
    }
    events.push(event(json!({"type":"content_block_stop","index":index})));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn request() -> Value {
        json!({"model":"local","messages":[{"role":"user","content":[{"type":"text","text":"Perform a web search for the query: Rust documentation"}]}],"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":8}]})
    }
    #[test]
    fn validates_native_request_and_domain_boundaries() {
        assert_eq!(
            Search::parse(&request()).unwrap().unwrap().query,
            "Rust documentation"
        );
        assert!(domain_matches(
            "https://docs.example.com/blog/post",
            "example.com/blog"
        ));
        assert!(!domain_matches(
            "https://evil-example.com/blog/post",
            "example.com/blog"
        ));
        assert!(!domain_matches(
            "https://example.com/blogger",
            "example.com/blog"
        ));
        for field in [
            json!({"user_location":{"country":"US"}}),
            json!({"allowed_domains":["example.com"],"blocked_domains":[]}),
            json!({"max_uses":-1}),
        ] {
            let mut body = request();
            body["tools"][0]
                .as_object_mut()
                .unwrap()
                .extend(field.as_object().unwrap().clone());
            assert!(Search::parse(&body).is_err());
        }
        let mut body = request();
        body["messages"][0]["content"] = json!("Unrelated conversation");
        assert!(Search::parse(&body).is_err());
    }
    #[tokio::test]
    async fn zero_budget_does_not_search() {
        let mut body = request();
        body["tools"][0]["max_uses"] = json!(0);
        let (blocks, uses) = Search::parse(&body)
            .unwrap()
            .unwrap()
            .results(&SearchProvider::Public, "call")
            .await;
        assert_eq!(uses, 0);
        assert_eq!(blocks[0]["content"]["error_code"], "max_uses_exceeded");
    }
}
