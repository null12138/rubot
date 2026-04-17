# Rubot

An autonomous AI agent built in Rust with hierarchical memory, multi-step planning, and reflective error learning.

Rubot runs a continuous **Think → Plan → Act → Reflect** loop powered by any OpenAI-compatible LLM. It can search the web, execute code, manage files, and learn from its own mistakes — all through a terminal REPL.

## Features

- **LLM-Agnostic** — works with any OpenAI-compatible API (OpenAI, Azure, local models via Ollama/vLLM/LM Studio, etc.)
- **Tool-Calling Architecture** — 6 built-in tools (web search, web fetch, code execution, file ops, skill create/get)
- **Hierarchical Memory** — 3-layer memory system (Working → Episodic → Semantic) with L0 index for fast lookup
- **Multi-Step Planning** — LLM generates JSON plans with step dependencies, executed sequentially with automatic retry
- **Error Book** — automatically records errors and their solutions; consults known fixes before retrying
- **Context Pruning** — when conversation exceeds token limits, older messages are summarized to keep context lean
- **Skill System** — create and load reusable skill templates stored as Markdown files
- **Session Persistence** — plans and execution logs survive across sessions; unfinished plans are detected on restart

## Quick Start

### Prerequisites

- Rust 1.70+ (edition 2021)
- An OpenAI-compatible API endpoint

### Install & Run

```bash
git clone <repo-url> rubot
cd rubot

# Configure environment
cp .env.example .env
# Edit .env with your API key and endpoint

cargo run
```

### Configuration

All settings are controlled via environment variables (`.env` file):

| Variable | Default | Description |
|---|---|---|
| `RUBOT_API_BASE_URL` | `https://api.openai.com/v1` | OpenAI-compatible API endpoint |
| `RUBOT_API_KEY` | `sk-placeholder` | API authentication key |
| `RUBOT_MODEL` | `gpt-4o` | Model name to use |
| `RUBOT_WORKSPACE` | `workspace` | Path to the workspace directory |
| `RUBOT_MAX_CONTEXT_TOKENS` | `120000` | Token limit before context pruning triggers |
| `RUBOT_MAX_RETRIES` | `3` | Max retry attempts for LLM calls and tool failures |
| `RUBOT_CODE_EXEC_TIMEOUT` | `30` | Timeout in seconds for code execution |

### Using with Local Models

Point `RUBOT_API_BASE_URL` to your local inference server:

```bash
# Ollama
RUBOT_API_BASE_URL=http://localhost:11434/v1
RUBOT_API_KEY=ollama
RUBOT_MODEL=llama3

# vLLM
RUBOT_API_BASE_URL=http://localhost:8000/v1
RUBOT_API_KEY=token-abc123
RUBOT_MODEL=meta-llama/Meta-Llama-3-70B
```

## Architecture

### Core Loop

```
┌─────────────────────────────────────────────────────────┐
│  User Input                                             │
│      │                                                  │
│      ▼                                                  │
│  ┌──────────┐   ┌──────────┐   ┌──────────┐           │
│  │  THINK   │──▶│   PLAN   │──▶│   ACT    │            │
│  │  (prune  │   │  (LLM    │   │  (tools  │            │
│  │  context)│   │  decide) │   │  execute)│            │
│  └──────────┘   └──────────┘   └──────────┘            │
│       │                              │                   │
│       │         ┌──────────┐         │                   │
│       └─────────│ REFLECT  │◀────────┘                  │
│                 │ (memory  │                             │
│                 │  +errors)│                             │
│                 └──────────┘                             │
└─────────────────────────────────────────────────────────┘
```

Each user message enters the agent loop. The LLM decides whether to call tools, generate a multi-step plan, or respond directly. Tool results feed back into the loop until the LLM produces a final answer.

### Project Structure

```
src/
├── main.rs              # Entry point, REPL loop
├── agent.rs             # Core agent: Think→Plan→Act→Reflect
├── config.rs            # Environment configuration
├── personality.rs       # System prompt template
│
├── llm/                 # LLM client
│   ├── mod.rs
│   ├── client.rs        # HTTP client with retry logic
│   └── types.rs         # Message, ToolCall, ChatRequest/Response types
│
├── tools/               # Tool implementations
│   ├── mod.rs
│   ├── registry.rs      # Dynamic tool registry (trait-based)
│   ├── web_search.rs    # DuckDuckGo HTML search
│   ├── web_fetch.rs     # URL → text/markdown converter
│   ├── code_exec.rs     # Bash/Python code execution
│   ├── file_ops.rs      # Read/write/list/append in workspace
│   ├── skill_create.rs  # Save reusable skill templates
│   └── skill_get.rs     # Load/list skill templates
│
├── memory/              # Hierarchical memory system
│   ├── mod.rs
│   ├── layer.rs         # L1 Working, L2 Episodic, L3 Semantic enums
│   ├── store.rs         # Markdown file I/O with YAML frontmatter
│   ├── index.rs         # L0 memory index (memory_index.md)
│   └── search.rs        # Quick (index) and deep (content) search
│
├── planner/             # Multi-step plan execution
│   ├── mod.rs
│   ├── chain.rs         # ToolCallChain with dependency graph
│   └── executor.rs      # Sequential executor with retry + error book
│
├── reflector/           # Reflection and error learning
│   ├── mod.rs
│   ├── error_book.rs    # Persistent error→solution mapping
│   └── matcher.rs       # Error pattern matching
│
├── state/               # Cross-session state
│   ├── mod.rs
│   └── manager.rs       # Plan persistence + execution log
│
└── context/             # Context window management
    ├── mod.rs
    └── cleaner.rs       # Token estimation + summarization pruning
```

Workspace layout (created at runtime):

```
workspace/
├── memory/
│   ├── memory_index.md   # L0 index: filename → summary mapping
│   ├── working/          # L1: current session notes (ephemeral)
│   ├── episodic/         # L2: past interaction summaries
│   └── semantic/         # L3: distilled long-term knowledge
├── errors/
│   └── error_book.md     # Known error patterns and solutions
├── skills/               # Reusable skill templates (.md)
└── state/
    ├── current_plan.md   # Active plan (if any)
    └── execution_log.md  # Tool call history with timestamps
```

## Key Components

### 1. Agent Loop (`src/agent.rs`)

The `Agent` struct orchestrates the entire workflow:

1. **Receive input** — push user message to conversation history
2. **Check context** — if token count exceeds 80% of `max_context_tokens`, prune via summarization
3. **Call LLM** — send full conversation + tool definitions to the API
4. **Handle response**:
   - **Tool calls** → execute each via `ToolRegistry`, append results, loop back
   - **Plan JSON** → parse into `ToolCallChain`, show to user, execute steps, synthesize final answer
   - **Direct text** → return to user
5. **Reflect** — if the loop ran multiple iterations, save a summary to working memory

The agent enforces a maximum of 20 iterations per request to prevent infinite tool-call loops.

### 2. LLM Client (`src/llm/`)

A lightweight HTTP client using `reqwest` that:

- Sends `ChatCompletion` requests with optional tool definitions
- Supports `tool_choice: "auto"` for function calling
- Retries on transient errors (429, 500, 502, 503, timeouts) with exponential backoff
- Estimates token usage via `chars / 4` heuristic

Compatible with any API that follows the OpenAI chat completions format.

### 3. Tools (`src/tools/`)

All tools implement the async `Tool` trait:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult>;
}
```

| Tool | Name | Description |
|---|---|---|
| Web Search | `web_search` | Searches DuckDuckGo (HTML scraping), returns titles, URLs, and snippets |
| Web Fetch | `web_fetch` | Fetches a URL and converts HTML→text, pretty-prints JSON, with length truncation |
| Code Execution | `code_exec` | Runs bash or Python code via subprocess with configurable timeout |
| File Operations | `file_ops` | Read, write, append, and list files within the workspace sandbox |
| Skill Create | `skill_create` | Saves a reusable skill as a Markdown file with YAML frontmatter |
| Skill Get | `skill_get` | Lists all skills or loads a specific one by name |

The `ToolRegistry` dynamically registers tools at startup and generates OpenAI-compatible tool definitions for the LLM.

### 4. Memory System (`src/memory/`)

A file-based hierarchical memory inspired by cognitive memory models:

```
L0  memory_index.md      Fast lookup: filename → one-line summary [+tags]
│
├── L1  working/          Current session scratch notes
│                         Priority: 3 (searched first)
│                         Lifecycle: cleared on session end
│
├── L2  episodic/         Summaries of past interactions
│                         Priority: 2
│                         Lifecycle: promoted from L1 on shutdown
│
└── L3  semantic/         Distilled long-term knowledge
                          Priority: 1 (searched last)
                          Lifecycle: permanent
```

Each memory entry is a Markdown file with YAML-like frontmatter:

```markdown
---
summary: Researched Rust async patterns
created: 2025-04-16T10:30:00+00:00
layer: Working
tags: rust, async, tokio
---

# Research Notes
...
```

**Search modes:**
- **Quick search** — scans the L0 index for keyword matches in summaries and tags
- **Deep search** — loads matching files and ranks by relevance (summary matches weighted 2x vs content matches)

### 5. Planning System (`src/planner/`)

When the LLM encounters a complex task, it outputs a JSON plan:

```json
{
  "type": "plan",
  "goal": "Research and summarize a topic",
  "steps": [
    {"tool": "web_search", "params": {"query": "topic"}, "description": "Search for topic"},
    {"tool": "web_fetch", "params": {"url": "$step_0.result"}, "description": "Fetch top result", "depends_on": [0]},
    {"tool": "file_ops", "params": {"action": "write", "path": "summary.md", "content": "$step_1.result"}, "description": "Save summary", "depends_on": [1]}
  ]
}
```

The `ChainExecutor`:
1. Finds the next step with all dependencies satisfied
2. Resolves `$step_N.result` references from previous step outputs
3. Executes the tool, retries on failure (up to `max_retries`)
4. Records errors to the Error Book
5. Continues until all steps are done or failed

Step status tracking: `[ ]` Pending → `[~]` Running → `[x]` Done / `[!]` Failed / `[-]` Skipped

### 6. Error Book (`src/reflector/`)

A persistent log of error patterns and solutions stored in `workspace/errors/error_book.md`:

```markdown
# Error Book

## [err_1] web_search error
- **patterns**: "429" OR "rate limit"
- **solution**: Wait and retry with backoff
- **seen**: 2025-04-16, 2025-04-17
```

When a tool call fails:
1. The error message is checked against known patterns
2. If matched, the known solution is returned as context to the LLM
3. If new, the error is auto-recorded with pattern extraction (HTTP status codes, key phrases like "timeout", "permission denied")
4. New entries start with a placeholder solution pending LLM classification

### 7. Context Pruning (`src/context/`)

When the estimated token count exceeds 80% of `max_context_tokens`:

1. Messages are split: system (keep) + old messages (summarize) + recent 12 messages (keep)
2. Old messages are sent to the LLM with a summarization prompt (max 300 words, focus on goals/decisions/results/errors)
3. The conversation is rebuilt: `[system] + [summary message] + [recent 12 messages]`

This allows Rubot to handle long-running sessions without running out of context.

### 8. State Manager (`src/state/`)

Persists state across sessions using Markdown files:

- `current_plan.md` — the active plan (if any), with step statuses
- `execution_log.md` — a table of all tool calls with timestamps and outcomes

On startup, the agent checks for unfinished plans (steps still marked `[ ]` or `[~]`) and logs a notice.

## REPL Commands

| Command | Description |
|---|---|
| `/quit` or `/exit` | Graceful shutdown (saves session memory) |
| `/plan` | Show the current active plan |
| `/memory` | Display the memory index |
| `/errors` | Show the error book |
| Any other text | Sent to the agent as a user message |

## Skill System

Skills are reusable templates stored as Markdown files in `workspace/skills/`:

```markdown
---
name: daily_report
description: Generate a daily summary report
trigger: when asked for a daily report or summary
---

# Daily Report Skill

1. Use `web_search` to find today's news on the topic
2. Use `web_fetch` to read the top 3 articles
3. Summarize findings into a structured report
4. Save to `reports/YYYY-MM-DD.md` via `file_ops`
```

Skills are created via the `skill_create` tool and loaded via `skill_get`. The LLM can create new skills during conversation and reference them later.

## Security

- **File sandboxing** — `file_ops` resolves all paths relative to the workspace directory and rejects paths that escape it
- **Code execution timeout** — shell/Python execution has a configurable timeout (default 30s)
- **API key isolation** — credentials are loaded from `.env` and never written to workspace files

## Dependencies

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime |
| `reqwest` | HTTP client for LLM API and web tools |
| `serde` / `serde_json` | Serialization |
| `clap` | CLI argument parsing |
| `anyhow` / `thiserror` | Error handling |
| `tracing` | Structured logging |
| `rustyline` | Terminal readline with history |
| `scraper` | HTML parsing for web search |
| `html2text` | HTML to text conversion |
| `chrono` | Timestamps |
| `uuid` | Unique IDs for memory entries |
| `regex` | Pattern matching |
| `dotenvy` | `.env` file loading |

## License

MIT
