//! Search providers used by Claude Code's native WebSearch compatibility path.
use crate::config::SearchProvider;
use anyhow::{bail, ensure, Context, Result};
use reqwest::Url;
use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

const MAX_BYTES: usize = 2 * 1024 * 1024;

pub async fn search(provider: &SearchProvider, query: &str, count: usize) -> Result<Vec<Value>> {
    ensure!(
        !query.trim().is_empty() && query.len() <= 2000,
        "query must contain 1–2000 bytes"
    );
    ensure!((1..=10).contains(&count), "count must be between 1 and 10");
    tokio::time::timeout(
        Duration::from_secs(30),
        search_inner(provider, query, count),
    )
    .await
    .context("web search timed out after 30 seconds")?
}

fn provider_request(
    client: &reqwest::Client,
    provider: &SearchProvider,
    query: &str,
    count: usize,
) -> Result<reqwest::RequestBuilder> {
    provider.validate()?;
    match provider {
        SearchProvider::Searxng { url } => {
            let mut url = Url::parse(url)?;
            let path = url.path().trim_end_matches('/');
            let path = if path.ends_with("/search") {
                path.to_owned()
            } else {
                format!("{path}/search")
            };
            url.set_path(&path);
            Ok(client.get(url).query(&[("q", query), ("format", "json")]))
        }
        SearchProvider::Brave { api_key } => {
            let mut key = reqwest::header::HeaderValue::from_str(api_key)?;
            key.set_sensitive(true);
            Ok(client
                .get("https://api.search.brave.com/res/v1/web/search")
                .query(&[("q", query), ("count", &count.to_string())])
                .header("X-Subscription-Token", key))
        }
        SearchProvider::Public => bail!("public search uses DuckDuckGo Lite"),
    }
}

async fn search_inner(provider: &SearchProvider, query: &str, count: usize) -> Result<Vec<Value>> {
    if matches!(provider, SearchProvider::Public) {
        let (_, _, text) = download(public_search_url(query)).await?;
        return search_results(&text, count);
    }
    // The user's configured SearXNG endpoint may be local or on a nonstandard port.
    // This exception never applies to search-result URLs.
    // No redirects: credentials and queries cannot be forwarded to another origin.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    let mut response = provider_request(&client, provider, query, count)?
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("could not reach search provider"))?;
    ensure!(response.status().is_success(), "search provider returned HTTP {}; check provider settings (SearXNG requires JSON output enabled)", response.status());
    ensure!(
        response.content_length().unwrap_or(0) <= MAX_BYTES as u64,
        "search response exceeds 2 MiB limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("search response interrupted"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_BYTES,
            "search response exceeds 2 MiB limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    let body: Value =
        serde_json::from_slice(&bytes).context("search provider returned invalid JSON")?;
    json_results(provider, &body, count)
}

fn json_results(provider: &SearchProvider, body: &Value, count: usize) -> Result<Vec<Value>> {
    let items = match provider {
        SearchProvider::Searxng { .. } => body.get("results"),
        SearchProvider::Brave { .. } => {
            // Brave omits the web group when there are no web matches.
            if body["type"] == "search" && body.get("web").is_none() {
                return Ok(Vec::new());
            }
            body.get("web").and_then(|web| web.get("results"))
        }
        SearchProvider::Public => bail!("public search uses DuckDuckGo Lite"),
    }
    .and_then(Value::as_array)
    .context("search provider response is missing results")?;
    let mut results = Vec::new();
    for item in items {
        let Some(raw_url) = item["url"].as_str() else {
            continue;
        };
        let Ok(url) = Url::parse(raw_url) else {
            continue;
        };
        if validate_url(&url).is_err() {
            continue;
        }
        let snippet = if matches!(provider, SearchProvider::Searxng { .. }) {
            &item["content"]
        } else {
            &item["description"]
        };
        results.push(json!({"title":item["title"].as_str().unwrap_or(""),"url":url.as_str(),"snippet":page_text(snippet.as_str().unwrap_or(""))}));
        if results.len() >= count {
            break;
        }
    }
    Ok(results)
}

fn validate_url(url: &Url) -> Result<()> {
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "only public HTTP(S) URLs are supported"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URLs containing credentials are not supported"
    );
    ensure!(
        matches!(url.port_or_known_default(), Some(80 | 443)),
        "only HTTP ports 80 and 443 are supported"
    );
    ensure!(url.host_str().is_some(), "URL requires a host");
    Ok(())
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || a == 0
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && matches!(b, 18 | 19)))
        }
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(ip));
            }
            let s = ip.segments();
            // Only global unicast; reject documentation, transition and special-use prefixes.
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
        }
    }
}

async fn download(mut url: Url) -> Result<(Url, String, String)> {
    for hop in 0..=5 {
        validate_url(&url)?;
        let host = url
            .host_str()
            .unwrap()
            .trim_start_matches('[')
            .trim_end_matches(']');
        let port = url.port_or_known_default().unwrap();
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .context("could not resolve web host")?
            .collect();
        ensure!(
            !addresses.is_empty() && addresses.iter().all(|address| public_ip(address.ip())),
            "only public internet addresses are allowed"
        );
        // Pin the checked DNS answers for this hop; never resolve again after validation.
        // A separate client ensures model credentials and proxy auth cannot reach a website.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &addresses)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .user_agent("Codeport/0.1 (public web tools)")
            .build()?;
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("could not fetch public page"))?;
        if response.status().is_redirection() {
            ensure!(hop < 5, "too many redirects");
            let location = response
                .headers()
                .get("location")
                .context("redirect has no location")?
                .to_str()?;
            url = url.join(location).context("invalid redirect URL")?;
            continue;
        }
        ensure!(
            response.status().is_success(),
            "website returned HTTP {}",
            response.status()
        );
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_BYTES as u64,
            "page exceeds 2 MiB download limit"
        );
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("page download interrupted"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= MAX_BYTES,
                "page exceeds 2 MiB download limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        return Ok((url, mime, String::from_utf8_lossy(&bytes).into_owned()));
    }
    unreachable!()
}

fn public_search_url(query: &str) -> Url {
    let mut url = Url::parse("https://lite.duckduckgo.com/lite/").unwrap();
    url.query_pairs_mut().append_pair("q", query);
    url
}

fn result_url(href: &str) -> Option<Url> {
    let base = Url::parse("https://lite.duckduckgo.com/").unwrap();
    let mut url = base.join(href).ok()?;
    if matches!(
        url.host_str(),
        Some("duckduckgo.com" | "www.duckduckgo.com" | "lite.duckduckgo.com")
    ) {
        if url.path() != "/l/" {
            return None;
        }
        let target = url
            .query_pairs()
            .find(|(key, _)| key == "uddg")?
            .1
            .into_owned();
        url = Url::parse(&target).ok()?;
    }
    validate_url(&url).ok()?;
    Some(url)
}

fn search_results(html: &str, count: usize) -> Result<Vec<Value>> {
    let document = Html::parse_document(html);
    let challenge =
        Selector::parse("#challenge-form, .anomaly-modal, form[action*='anomaly.js']").unwrap();
    ensure!(document.select(&challenge).next().is_none(), "public search provider returned a bot challenge; retry later or configure SearXNG or Brave with codeport -cfg");
    // Lite places each snippet in the row after its result link. Walk both in
    // document order so a missing/invalid result cannot shift snippet associations.
    let selector = Selector::parse("a.result-link, .result-snippet").unwrap();
    let mut results: Vec<Value> = Vec::new();
    let mut current = None;
    let mut seen = std::collections::HashSet::new();
    let mut found_links = false;
    for node in document.select(&selector) {
        if node.value().name() == "a" {
            found_links = true;
            current = None;
            let Some(url) = node.value().attr("href").and_then(result_url) else {
                continue;
            };
            let title = node
                .text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if title.is_empty() || !seen.insert(url.to_string()) {
                continue;
            }
            current = Some(results.len());
            results.push(json!({"title":title,"url":url.as_str(),"snippet":""}));
        } else if let Some(index) = current.take() {
            results[index]["snippet"] = json!(page_text(&node.inner_html()));
        }
    }
    if results.is_empty() {
        let empty = Selector::parse(".no-results, .no-results__message").unwrap();
        ensure!(!found_links && document.select(&empty).next().is_some(), "public search returned an unrecognized page or no usable result links; retry later or configure SearXNG or Brave with codeport -cfg");
    }
    results.truncate(count);
    Ok(results)
}

fn page_text(html: &str) -> String {
    let document = Html::parse_document(html);
    let selector = Selector::parse("body").unwrap();
    let root = document
        .select(&selector)
        .next()
        .unwrap_or_else(|| document.root_element());
    let mut output = String::new();
    for node in root.descendants() {
        if let Some(text) = node.value().as_text() {
            if node.ancestors().any(|parent| {
                parent.value().as_element().is_some_and(|el| {
                    matches!(
                        el.name(),
                        "script" | "style" | "noscript" | "svg" | "template"
                    )
                })
            }) {
                continue;
            }
            for word in text.split_whitespace() {
                if !output.is_empty() {
                    output.push(' ');
                }
                output.push_str(word);
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_requests_and_response_formats() {
        let client = reqwest::Client::new();
        let brave = SearchProvider::Brave {
            api_key: "test-secret".into(),
        };
        let request = provider_request(&client, &brave, "rust & tools", 3)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().host_str(), Some("api.search.brave.com"));
        assert_eq!(request.headers()["X-Subscription-Token"], "test-secret");
        assert!(request.headers()["X-Subscription-Token"].is_sensitive());
        assert!(!format!("{request:?}").contains("test-secret"));
        assert!(request
            .url()
            .query_pairs()
            .any(|(k, v)| k == "q" && v == "rust & tools"));
        assert!(request
            .url()
            .query_pairs()
            .any(|(k, v)| k == "count" && v == "3"));
        let result = json_results(&brave, &json!({"web":{"results":[{"title":"Rust","url":"https://rust-lang.org","description":"<b>Rust</b> docs"}]}}),3).unwrap();
        assert_eq!(result[0]["snippet"], "Rust docs");
        assert!(json_results(&brave, &json!({"type":"search"}), 3)
            .unwrap()
            .is_empty());
        assert!(json_results(&brave, &json!({"error":"bad key"}), 3).is_err());
        for base in [
            "http://localhost:8080/searx",
            "http://localhost:8080/searx/",
            "http://localhost:8080/searx/search",
        ] {
            let provider = SearchProvider::Searxng { url: base.into() };
            let request = provider_request(&client, &provider, "q", 5)
                .unwrap()
                .build()
                .unwrap();
            assert_eq!(request.url().path(), "/searx/search");
            assert!(request
                .url()
                .query_pairs()
                .any(|(k, v)| k == "format" && v == "json"));
            assert!(request.headers().get("X-Subscription-Token").is_none());
        }
    }

    #[test]
    fn rejects_local_and_special_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.0.102",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public_ip(address.parse().unwrap()));
        }
        for url in [
            "file:///etc/passwd",
            "http://user:secret@example.com",
            "https://example.com:8080",
        ] {
            assert!(validate_url(&Url::parse(url).unwrap()).is_err());
        }
    }
    #[test]
    fn parses_lite_results_and_preserves_url_and_snippet_associations() {
        let html = r#"<table>
<tr><td><a class='result-link' href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fcfd%3Fa%3D1%26b%3D2&amp;rut=tracking'>CFD Broker Risk Warnings</a></td></tr>
<tr><td class='result-snippet'>The <b>percentage</b> of retail accounts that lose capital.</td></tr>
<tr><td><a class='result-link' href='javascript:bad()'>Invalid</a></td></tr>
<tr><td class='result-snippet'>Must not attach to the first result.</td></tr>
<tr><td><a class='result-link' href='https://example.org/'>Retail loss disclosures</a></td></tr>
<tr><td class='result-snippet'>Broker-specific rates &amp; warnings.</td></tr>
</table>"#;
        let results = search_results(html, 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["url"], "https://example.com/cfd?a=1&b=2");
        assert_eq!(
            results[0]["snippet"],
            "The percentage of retail accounts that lose capital."
        );
        assert_eq!(results[1]["snippet"], "Broker-specific rates & warnings.");
        assert_eq!(search_results(html, 1).unwrap().len(), 1);
        assert_eq!(page_text("<body>Hello <b>world</b><script>secret()</script><style>bad</style><p>Next &amp; last</p></body>"),"Hello world Next & last");
    }

    #[test]
    fn distinguishes_challenges_empty_results_and_invalid_pages() {
        assert!(
            search_results("<form id='challenge-form'>Select ducks</form>", 5)
                .unwrap_err()
                .to_string()
                .contains("bot challenge")
        );
        assert!(search_results("<html>Something changed</html>", 5).is_err());
        assert!(
            search_results("<div class='no-results'>No results found</div>", 5)
                .unwrap()
                .is_empty()
        );
        assert!(search_results("<a class='result-link' href='file:///tmp/a'>Bad</a>", 5).is_err());
    }

    #[test]
    fn public_search_preserves_entire_query() {
        let query = "CFD broker warning percentage of retail accounts lose capit.. & \"risk\"";
        let url = public_search_url(query);
        assert_eq!(url.host_str(), Some("lite.duckduckgo.com"));
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("q".into(), query.into())]
        );
    }
    #[tokio::test]
    async fn validates_queries_before_network() {
        assert!(search(&SearchProvider::Public, "", 5).await.is_err());
        assert!(search(&SearchProvider::Public, "test", 11).await.is_err());
    }
}
