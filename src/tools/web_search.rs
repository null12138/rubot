use anyhow::Result;
use async_trait::async_trait;
use scraper::{Html, Selector};
use super::registry::{Tool, ToolResult};

static UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub struct WebSearch;

fn decode_bing_url(raw: &str) -> String {
    let normalized = raw.replace("&amp;", "&");
    let decoded: String = urlencoding::decode(&normalized).map(|s| s.into_owned()).unwrap_or(normalized);
    if let Some(idx) = decoded.find("&u=") {
        let encoded = &decoded[idx + 3..];
        let stripped = encoded.strip_prefix('a').unwrap_or(encoded);
        let stripped = stripped.strip_prefix('1').unwrap_or(stripped);
        if stripped.len() > 10 {
            if let Ok(bytes) = data_encoding::BASE64URL.decode(stripped.as_bytes()) {
                if let Ok(s) = String::from_utf8(bytes) {
                    if s.starts_with("http") { return s; }
                }
            }
        }
    }
    raw.into()
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str { "web_search" }
    fn description(&self) -> &str { "Search the web." }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}, "max": {"type": "integer"}}, "required": ["query"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let q = params["query"].as_str().unwrap_or("");
        let max = params["max"].as_u64().unwrap_or(5).min(10) as usize;

        let url = format!("https://www.bing.com/search?q={}&setlang=en", urlencoding::encode(q));
        let client = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let res = client.get(&url).send().await?.text().await?;

        let doc = Html::parse_document(&res);
        let mut results = vec![];
        let algo = Selector::parse("li.b_algo").unwrap();
        let h2 = Selector::parse("h2").unwrap();
        let a_sel = Selector::parse("a").unwrap();
        let p_sel = Selector::parse("p.b_lineclamp").unwrap();
        let p_any = Selector::parse("p").unwrap();

        for r in doc.select(&algo).take(max) {
            if let Some(h) = r.select(&h2).next() {
                if let Some(a) = h.select(&a_sel).next() {
                    let title = a.text().collect::<String>();
                    let href = a.value().attr("href").unwrap_or("");
                    let link = decode_bing_url(href);
                    let snippet = r.select(&p_sel).next()
                        .or_else(|| r.select(&p_any).next())
                        .map(|e| e.text().collect::<String>())
                        .unwrap_or_default();
                    if !title.trim().is_empty() {
                        results.push(format!("[{}]({})\n{}", title.trim(), link, snippet.trim()));
                    }
                }
            }
        }
        Ok(ToolResult::ok(if results.is_empty() { "No results".into() } else { results.join("\n\n") }))
    }
}
