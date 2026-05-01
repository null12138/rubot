use super::registry::{RiskLevel, Tool, ToolResult};
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Url;
use scraper::{Html, Selector};
use serde::Deserialize;

static UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_CANDIDATES: usize = 20;
const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";

pub struct WebSearch;

#[derive(Debug, Clone)]
struct SearchCandidate {
    title: String,
    link: String,
    snippet: String,
    host: String,
}

#[derive(Debug, Clone, Default)]
struct SearchConstraints {
    required_hosts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

/// Unwrap Bing's click-tracking wrapper of the form
/// `https://www.bing.com/ck/a?!&&p=...&u=a1<BASE64URL>&ntb=1&...`
/// into the real target URL. Falls back to the raw href.
fn decode_bing_url(raw: &str) -> String {
    let normalized = raw.replace("&amp;", "&");
    let decoded: String = urlencoding::decode(&normalized)
        .map(|s| s.into_owned())
        .unwrap_or(normalized);
    if let Some(encoded) = extract_bing_u_param(&decoded) {
        // Trim to the next `&` — Bing appends more query params after the
        // base64 payload (`&ntb=1&...`) and the old decoder was trying to
        // base64-decode the whole tail, silently failing every time.
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

fn extract_bing_u_param(decoded: &str) -> Option<&str> {
    let idx = decoded.find("?u=").or_else(|| decoded.find("&u="))?;
    let tail = &decoded[idx + 3..];
    let end = tail.find('&').unwrap_or(tail.len());
    Some(&tail[..end])
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web via Tavily when configured, with Bing as fallback."
    }
    fn is_concurrency_safe(&self) -> bool { true }
    fn risk_level(&self) -> RiskLevel { RiskLevel::Low }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}, "max": {"type": "integer"}}, "required": ["query"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let q = params["query"].as_str().unwrap_or("");
        let max = params["max"].as_u64().unwrap_or(5).min(10) as usize;
        if let Some(reason) = risky_query_reason(q) {
            return Ok(ToolResult::err(reason));
        }
        let constraints = parse_constraints(q);
        let mut backend_errors = Vec::new();

        let tavily_api_key = std::env::var("RUBOT_TAVILY_API_KEY").unwrap_or_default();
        let candidates = if tavily_api_key.trim().is_empty() {
            match search_bing(q).await {
                Ok(candidates) => candidates,
                Err(err) => {
                    return Ok(ToolResult::err(format!("web_search failed: {err:#}")));
                }
            }
        } else {
            match search_tavily(q, &constraints, &tavily_api_key).await {
                Ok(candidates) if !candidates.is_empty() => candidates,
                Ok(_) => {
                    backend_errors.push("Tavily returned no results".to_string());
                    match search_bing(q).await {
                        Ok(candidates) => candidates,
                        Err(err) => {
                            backend_errors.push(format!("Bing fallback failed: {err:#}"));
                            return Ok(ToolResult::err(backend_errors.join("\n")));
                        }
                    }
                }
                Err(err) => {
                    backend_errors.push(format!("Tavily failed: {err:#}"));
                    match search_bing(q).await {
                        Ok(candidates) => candidates,
                        Err(bing_err) => {
                            backend_errors.push(format!("Bing fallback failed: {bing_err:#}"));
                            return Ok(ToolResult::err(backend_errors.join("\n")));
                        }
                    }
                }
            }
        };

        let results = rank_candidates(q, candidates, max)
            .into_iter()
            .map(|item| format!("[{}]({})\n{}", item.title, item.link, item.snippet))
            .collect::<Vec<_>>();
        Ok(ToolResult::ok(if results.is_empty() {
            "No results".into()
        } else {
            results.join("\n\n")
        }))
    }
}

async fn search_tavily(
    query: &str,
    constraints: &SearchConstraints,
    api_key: &str,
) -> Result<Vec<SearchCandidate>> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let mut body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "topic": "general",
        "search_depth": "basic",
        "max_results": MAX_CANDIDATES.min(10),
        "include_answer": false,
        "include_images": false,
        "include_raw_content": false,
    });
    if !constraints.required_hosts.is_empty() {
        body["include_domains"] = serde_json::json!(constraints.required_hosts);
    }

    let response = client
        .post(TAVILY_ENDPOINT)
        .json(&body)
        .send()
        .await
        .context("request to Tavily failed")?;

    let status = response.status();
    let text = response
        .text()
        .await
        .context("failed to read Tavily body")?;
    if !status.is_success() {
        anyhow::bail!("Tavily HTTP {}: {}", status, text);
    }

    let parsed: TavilyResponse =
        serde_json::from_str(&text).context("failed to parse Tavily response")?;
    Ok(parsed
        .results
        .into_iter()
        .filter_map(|result| build_candidate(result.title, result.url, result.content))
        .collect())
}

async fn search_bing(query: &str) -> Result<Vec<SearchCandidate>> {
    // Force US/English region so Bing doesn't geo-redirect mainland
    // China IPs to cn.bing.com and return stale Chinese news caches.
    //   mkt=en-US      — market
    //   cc=us          — country code
    //   setlang=en-US  — UI language
    //   ensearch=1     — disable auto language detection
    let url = format!(
        "https://www.bing.com/search?q={}&mkt=en-US&cc=us&setlang=en-US&ensearch=1",
        urlencoding::encode(query)
    );
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let body = client
        .get(&url)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .context("request to Bing failed")?
        .text()
        .await
        .context("failed to read Bing body")?;

    let doc = Html::parse_document(&body);
    let algo = Selector::parse("li.b_algo").unwrap();
    let h2 = Selector::parse("h2").unwrap();
    let a_sel = Selector::parse("a").unwrap();
    let p_lineclamp = Selector::parse("p.b_lineclamp").unwrap();
    let p_any = Selector::parse("p").unwrap();

    let mut candidates = vec![];
    for r in doc.select(&algo).take(MAX_CANDIDATES) {
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
                if let Some(candidate) = build_candidate(title, link, snippet) {
                    candidates.push(candidate);
                }
            }
        }
    }
    Ok(candidates)
}

fn build_candidate(title: String, link: String, snippet: String) -> Option<SearchCandidate> {
    let title = squash_ws(&title);
    let link = squash_ws(&link);
    let snippet = squash_ws(&snippet);
    if title.is_empty() || link.is_empty() {
        return None;
    }
    let url = Url::parse(&link).ok()?;
    let host = url
        .host_str()?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    if host.is_empty() || host.ends_with("bing.com") {
        return None;
    }
    Some(SearchCandidate {
        title,
        link,
        snippet,
        host,
    })
}

fn rank_candidates(
    query: &str,
    candidates: Vec<SearchCandidate>,
    max: usize,
) -> Vec<SearchCandidate> {
    let query_tokens = tokenize(query);
    let constraints = parse_constraints(query);
    let ascii_query = is_mostly_ascii(query);
    let mut scored = candidates
        .into_iter()
        .filter_map(|candidate| {
            let score = score_candidate(&candidate, &query_tokens, &constraints, ascii_query);
            (score > -100).then_some((score, candidate))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.host.cmp(&b.1.host))
            .then_with(|| a.1.title.cmp(&b.1.title))
    });

    let mut seen = std::collections::HashSet::<String>::new();
    let mut out = Vec::new();
    for (_, candidate) in scored {
        let dedupe_key = format!(
            "{}|{}",
            candidate.host,
            candidate.title.to_ascii_lowercase()
        );
        if seen.insert(dedupe_key) {
            out.push(candidate);
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

fn score_candidate(
    candidate: &SearchCandidate,
    query_tokens: &[String],
    constraints: &SearchConstraints,
    ascii_query: bool,
) -> i32 {
    let title = candidate.title.to_ascii_lowercase();
    let snippet = candidate.snippet.to_ascii_lowercase();
    let host = candidate.host.as_str();
    let mut score = 0i32;

    if !constraints.required_hosts.is_empty() {
        if host_matches_required(host, &constraints.required_hosts) {
            score += 120;
        } else {
            score -= 140;
        }
    }

    for token in query_tokens {
        if title.contains(token) {
            score += 8;
        }
        if snippet.contains(token) {
            score += 3;
        }
        if host.contains(token) {
            score += 6;
        }
    }

    if host.ends_with(".edu")
        || host.ends_with(".gov")
        || host.ends_with(".org")
        || host.contains("ssrn.com")
        || host.contains("arxiv.org")
        || host.contains("github.com")
    {
        score += 8;
    }

    if is_low_quality_host(host) {
        score -= if ascii_query { 60 } else { 15 };
    }

    if ascii_query && contains_cjk(&candidate.title) {
        score -= 35;
    }
    if ascii_query && contains_cjk(&candidate.snippet) {
        score -= 15;
    }

    if candidate.link.contains("/ck/a?") {
        score -= 20;
    }

    score
}

fn parse_constraints(query: &str) -> SearchConstraints {
    let required_hosts = query
        .split_whitespace()
        .filter_map(|part| {
            let lower = part.to_ascii_lowercase();
            let host = lower.strip_prefix("site:")?;
            let host = host
                .trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | ',' | ';'))
                .trim_end_matches('.');
            (!host.is_empty()).then(|| host.trim_start_matches("www.").to_string())
        })
        .collect();
    SearchConstraints { required_hosts }
}

fn host_matches_required(host: &str, required_hosts: &[String]) -> bool {
    required_hosts
        .iter()
        .any(|required| host == required || host.ends_with(&format!(".{}", required)))
}

fn is_low_quality_host(host: &str) -> bool {
    [
        "zhihu.com",
        "zhuanlan.zhihu.com",
        "zhidao.baidu.com",
        "tieba.baidu.com",
        "wenku.baidu.com",
        "csdn.net",
        "bilibili.com",
        "xiaohongshu.com",
        "weather.com.cn",
        "btc123.fans",
        "zcjsj8.com",
        "pan.baidu.com",
        "quark.cn",
        "aliyundrive.com",
        "doc88.com",
    ]
    .iter()
    .any(|suffix| host == *suffix || host.ends_with(&format!(".{}", suffix)))
}

fn risky_query_reason(query: &str) -> Option<String> {
    let lower = query.to_ascii_lowercase();
    let has_netdisk_marker = [
        "百度网盘",
        "夸克网盘",
        "阿里云盘",
        "pan.baidu.com",
        "quark",
        "aliyundrive",
        "123pan",
        "torrent",
        "magnet",
        "ed2k",
    ]
    .iter()
    .any(|needle| query.contains(needle) || lower.contains(needle));
    let has_free_pdf_mirror_pattern = [
        "free download pdf",
        "pdf free download",
        "免费下载 pdf",
        "pdf 免费下载",
        "资源下载",
        "免登录下载",
    ]
    .iter()
    .any(|needle| lower.contains(needle) || query.contains(needle));

    (has_netdisk_marker || has_free_pdf_mirror_pattern).then(|| {
        "Refusing low-quality or pirated-distribution search query. Do not search for 百度网盘 / 夸克网盘 / free-download PDF mirrors. Prefer official or otherwise authorized sources.".into()
    })
}

fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| part.len() >= 2)
        .filter(|part| !is_search_operator(part))
        .collect()
}

fn is_search_operator(token: &str) -> bool {
    matches!(
        token,
        "site"
            | "filetype"
            | "intitle"
            | "inurl"
            | "related"
            | "cache"
            | "or"
            | "and"
            | "pdf"
            | "www"
            | "com"
            | "org"
            | "net"
    )
}

fn is_mostly_ascii(text: &str) -> bool {
    let chars = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<Vec<_>>();
    if chars.is_empty() {
        return true;
    }
    let ascii = chars.iter().filter(|c| c.is_ascii()).count();
    ascii * 100 / chars.len() >= 80
}

fn contains_cjk(text: &str) -> bool {
    text.chars().any(|c| matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF | 0xAC00..=0xD7AF))
}

fn squash_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

    #[test]
    fn bing_url_decoder_handles_query_prefix_u_param() {
        let encoded = data_encoding::BASE64URL.encode(b"https://example.org/test");
        let raw = format!("https://www.bing.com/ck/a?u=a1{}&ntb=1", encoded);
        assert_eq!(decode_bing_url(&raw), "https://example.org/test");
    }

    #[test]
    fn low_quality_cjk_results_are_filtered_for_ascii_queries() {
        let candidates = vec![
            SearchCandidate {
                title: "文章被收到SSRN上会对后续投稿有影响吗? - 知乎".into(),
                link: "https://www.zhihu.com/question/1".into(),
                snippet: "中文问答聚合页".into(),
                host: "zhihu.com".into(),
            },
            SearchCandidate {
                title: "SSRN Search Results".into(),
                link: "https://www.ssrn.com/en/search-results/".into(),
                snippet: "Official SSRN search page".into(),
                host: "ssrn.com".into(),
            },
        ];
        let ranked = rank_candidates(
            r#"SSRN "free download" PDF "abstract_id" "Delivery.cfm""#,
            candidates,
            5,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].host, "ssrn.com");
    }

    #[test]
    fn chinese_queries_do_not_drop_chinese_results() {
        let candidate = SearchCandidate {
            title: "怎么查看以往的热搜？ - 知乎".into(),
            link: "https://www.zhihu.com/question/2".into(),
            snippet: "中文结果".into(),
            host: "zhihu.com".into(),
        };
        let ranked = rank_candidates("怎么看热搜", vec![candidate], 5);
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn site_constraint_prefers_matching_host() {
        let ranked = rank_candidates(
            r#"site:arxiv.org "artificial intelligence" 2025"#,
            vec![
                SearchCandidate {
                    title: "Artificial Definition & Meaning".into(),
                    link: "https://www.merriam-webster.com/dictionary/artificial".into(),
                    snippet: "Dictionary page".into(),
                    host: "merriam-webster.com".into(),
                },
                SearchCandidate {
                    title: "cs.AI recent submissions".into(),
                    link: "https://arxiv.org/list/cs.AI/recent".into(),
                    snippet: "Recent arXiv submissions".into(),
                    host: "arxiv.org".into(),
                },
            ],
            5,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].host, "arxiv.org");
    }

    #[test]
    fn tokenize_ignores_search_operators() {
        let tokens = tokenize(r#"site:ssrn.com "free download" filetype:pdf OR abstract_id"#);
        assert!(!tokens.contains(&"site".into()));
        assert!(!tokens.contains(&"pdf".into()));
        assert!(!tokens.contains(&"or".into()));
        assert!(tokens.contains(&"ssrn".into()));
        assert!(tokens.contains(&"abstract".into()));
    }

    #[test]
    fn risky_queries_are_blocked() {
        let reason = risky_query_reason("教资真题 PDF 百度网盘 夸克网盘 下载").unwrap();
        assert!(reason.contains("pirated-distribution"));
        assert!(reason.contains("百度网盘"));
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
