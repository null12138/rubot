use anyhow::Result;
use async_trait::async_trait;
use scraper::{Html, Selector};

use super::registry::{Tool, ToolResult};

pub struct WebSearch;

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using DuckDuckGo. Returns titles, URLs, and snippets."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 5)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let query = match params["query"].as_str() {
            Some(q) if !q.is_empty() => q,
            _ => return Ok(ToolResult::err("Missing 'query' parameter".to_string())),
        };
        let max_results = params["max_results"].as_u64().unwrap_or(5) as usize;

        match search_ddg(query, max_results).await {
            Ok(results) => {
                if results.is_empty() {
                    Ok(ToolResult::ok("No results found.".to_string()))
                } else {
                    let formatted: Vec<String> = results
                        .iter()
                        .enumerate()
                        .map(|(i, r)| {
                            format!(
                                "{}. [{}]({})\n   {}",
                                i + 1,
                                r.title,
                                r.url,
                                r.snippet
                            )
                        })
                        .collect();
                    Ok(ToolResult::ok(formatted.join("\n\n")))
                }
            }
            Err(e) => Ok(ToolResult::err(format!("Search failed: {}", e))),
        }
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

async fn search_ddg(query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .build()?;

    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );

    let body = client.get(&url).send().await?.text().await?;
    let document = Html::parse_document(&body);

    let result_sel = Selector::parse(".result").unwrap();
    let title_sel = Selector::parse(".result__a").unwrap();
    let snippet_sel = Selector::parse(".result__snippet").unwrap();

    let mut results = Vec::new();
    for result in document.select(&result_sel).take(max_results) {
        let title = result
            .select(&title_sel)
            .next()
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default()
            .trim()
            .to_string();

        let href = result
            .select(&title_sel)
            .next()
            .and_then(|e| e.value().attr("href"))
            .unwrap_or("")
            .to_string();

        // DDG wraps URLs in redirect links — extract the actual URL
        let url = extract_ddg_url(&href).unwrap_or(href);

        let snippet = result
            .select(&snippet_sel)
            .next()
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default()
            .trim()
            .to_string();

        if !title.is_empty() {
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
        }
    }

    Ok(results)
}

fn extract_ddg_url(href: &str) -> Option<String> {
    // DDG format: //duckduckgo.com/l/?uddg=https%3A%2F%2F...&rut=...
    if let Some(start) = href.find("uddg=") {
        let rest = &href[start + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        let encoded = &rest[..end];
        urlencoding::decode(encoded).ok().map(|s| s.to_string())
    } else {
        None
    }
}
