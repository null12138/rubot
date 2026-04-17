use anyhow::Result;
use async_trait::async_trait;
use scraper::{Html, Selector};
use super::registry::{Tool, ToolResult};

pub struct WebSearch;

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str { "web_search" }
    fn description(&self) -> &str { "Fast DuckDuckGo search." }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}, "max": {"type": "integer"}}, "required": ["query"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let q = params["query"].as_str().unwrap_or("");
        let max = params["max"].as_u64().unwrap_or(5).min(10) as usize;
        let client = reqwest::Client::builder().user_agent("Mozilla/5.0").timeout(std::time::Duration::from_secs(10)).build()?;
        let res = client.get(format!("https://html.duckduckgo.com/html/?q={}", urlencoding::encode(q))).send().await?.text().await?;
        
        let doc = Html::parse_document(&res);
        let mut results = vec![];
        let sel = Selector::parse(".result").unwrap();
        let a_sel = Selector::parse(".result__a").unwrap();
        let s_sel = Selector::parse(".result__snippet").unwrap();

        for r in doc.select(&sel).take(max) {
            if let Some(a) = r.select(&a_sel).next() {
                let title = a.text().collect::<String>();
                let href = a.value().attr("href").unwrap_or("");
                let url = if let Some(s) = href.find("uddg=") {
                    let rest = &href[s+5..];
                    urlencoding::decode(&rest[..rest.find('&').unwrap_or(rest.len())]).ok().map(|s| s.into_owned()).unwrap_or(href.into())
                } else { href.into() };
                let snippet = r.select(&s_sel).next().map(|e| e.text().collect::<String>()).unwrap_or_default();
                results.push(format!("[{}]({})\n{}", title.trim(), url, snippet.trim()));
            }
        }
        Ok(ToolResult::ok(if results.is_empty() { "No results".into() } else { results.join("\n\n") }))
    }
}
