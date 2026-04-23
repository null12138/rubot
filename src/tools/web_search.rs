use super::registry::{Tool, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use scraper::{Html, Selector};

static UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub struct WebSearch;

/// Unwrap Bing's click-tracking wrapper of the form
/// `https://www.bing.com/ck/a?!&&p=...&u=a1<BASE64URL>&ntb=1&...`
/// into the real target URL. Falls back to the raw href.
fn decode_bing_url(raw: &str) -> String {
    let normalized = raw.replace("&amp;", "&");
    let decoded: String = urlencoding::decode(&normalized)
        .map(|s| s.into_owned())
        .unwrap_or(normalized);
    if let Some(idx) = decoded.find("&u=") {
        // Trim to the next `&` — Bing appends more query params after the
        // base64 payload (`&ntb=1&...`) and the old decoder was trying to
        // base64-decode the whole tail, silently failing every time.
        let tail = &decoded[idx + 3..];
        let end = tail.find('&').unwrap_or(tail.len());
        let encoded = &tail[..end];
        let stripped = encoded.strip_prefix('a').unwrap_or(encoded);
        let stripped = stripped.strip_prefix('1').unwrap_or(stripped);
        if stripped.len() > 10 {
            if let Ok(bytes) = data_encoding::BASE64URL.decode(stripped.as_bytes()) {
                if let Ok(s) = String::from_utf8(bytes) {
                    if s.starts_with("http") {
                        return s;
                    }
                }
            }
        }
    }
    raw.into()
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web via Bing in US English."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}, "max": {"type": "integer"}}, "required": ["query"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let q = params["query"].as_str().unwrap_or("");
        let max = params["max"].as_u64().unwrap_or(5).min(10) as usize;

        // Force US/English region so Bing doesn't geo-redirect mainland
        // China IPs to cn.bing.com and return stale Chinese news caches.
        //   mkt=en-US      — market
        //   cc=us          — country code
        //   setlang=en-US  — UI language
        //   ensearch=1     — disable auto language detection
        let url = format!(
            "https://www.bing.com/search?q={}&mkt=en-US&cc=us&setlang=en-US&ensearch=1",
            urlencoding::encode(q)
        );
        let client = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let body = client
            .get(&url)
            // Explicit Accept-Language backs up mkt=en-US for edge cases
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await?
            .text()
            .await?;

        let doc = Html::parse_document(&body);
        let algo = Selector::parse("li.b_algo").unwrap();
        let h2 = Selector::parse("h2").unwrap();
        let a_sel = Selector::parse("a").unwrap();
        let p_lineclamp = Selector::parse("p.b_lineclamp").unwrap();
        let p_any = Selector::parse("p").unwrap();

        let mut results = vec![];
        for r in doc.select(&algo).take(max) {
            if let Some(h) = r.select(&h2).next() {
                if let Some(a) = h.select(&a_sel).next() {
                    let title = a.text().collect::<String>();
                    let href = a.value().attr("href").unwrap_or("");
                    let link = decode_bing_url(href);
                    let snippet = r
                        .select(&p_lineclamp)
                        .next()
                        .or_else(|| r.select(&p_any).next())
                        .map(|e| e.text().collect::<String>())
                        .unwrap_or_default();
                    if !title.trim().is_empty() {
                        results.push(format!("[{}]({})\n{}", title.trim(), link, snippet.trim()));
                    }
                }
            }
        }
        Ok(ToolResult::ok(if results.is_empty() {
            "No results".into()
        } else {
            results.join("\n\n")
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bing_url_decoder_keeps_direct_links() {
        let direct = "https://example.com/page";
        assert_eq!(decode_bing_url(direct), direct);
    }

    #[test]
    fn bing_url_decoder_unwraps_base64url() {
        // "a1" prefix + base64url("https://example.com/")
        let encoded = data_encoding::BASE64URL.encode(b"https://example.com/");
        let raw = format!("https://www.bing.com/ck/a?!&&p=xx&u=a1{}&ntb=1", encoded);
        assert_eq!(decode_bing_url(&raw), "https://example.com/");
    }

    /// Live hit against Bing. Not run by default (slow + network).
    /// Run with: `cargo test --release bing_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn bing_live_returns_en_results() {
        let out = WebSearch
            .execute(serde_json::json!({"query": "bitcoin price", "max": 3}))
            .await
            .expect("execute returned Err");
        assert!(out.success, "expected success, got {:?}", out.error);
        println!("--- Bing live result ---\n{}\n---", out.output);
        assert!(!out.output.is_empty());
        assert_ne!(out.output, "No results");
        assert!(out.output.contains("](http"));
    }
}
