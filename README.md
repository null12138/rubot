# Rubot

Minimal autonomous AI agent in Rust — LLM + core tools + flat-file memory. A Think→Act loop that runs in a terminal REPL. Supports WeChat as an optional message channel.

## Features

- **LLM-agnostic** — any OpenAI-compatible API (OpenAI, Zhipu, DeepSeek, Ollama, vLLM, etc.)
- **Built-in tools** — `web_search`, `web_fetch`, `browser` (headless Chromium), `code_exec`, `file_ops`, `latex_pdf`
- **Subagents** — `subagent_spawn` / `subagent_wait` / `subagent_list` / `subagent_close` for parallel background work; uses fast model by default, with optional timeout
- **MD-backed tools** — `tool_create` / `tool_delete` to crystallize working solutions into reusable tools at runtime
- **Three-tier memory** — `memory_search` / `memory_add` / `memory_touch` / `memory_due` with Ebbinghaus spacing (Working → Episodic → Semantic); the agent actively reads and writes memory during tasks
- **Sleep/dream mode** — after idle time, a cheap LLM (OpenRouter free models if `RUBOT_ORKEY` is set, otherwise your fast model) silently consolidates working memories: merges related entries, archives patterns, evicts trivia
- **Multi-step plans** — LLM emits JSON plans that execute sequentially with dependency resolution
- **Auto-loop mode** — `/loop <task>|<stop>` drives a task with a stop condition
- **Heavy/fast model split** — first turn uses the heavy model, follow-ups use the fast model
- **CWD-based execution** — `code_exec` runs in the directory where you launched rubot, not a sandbox
- **Safety confirmation** — agent asks before destructive actions (delete, install packages, git mutations)
- **WeChat channel** — optional WeChat bot via `rubot wechat` (QR login, file/media delivery)

## Quick start

### One-line install (Linux / macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/null12138/rubot/main/install.sh | bash
```

If you hit permission errors, re-run with `sudo`:

```bash
curl -fsSL https://raw.githubusercontent.com/null12138/rubot/main/install.sh | sudo bash
```

### Windows

```powershell
irm https://raw.githubusercontent.com/null12138/rubot/main/install.ps1 | iex
```

### First run

```bash
rubot
```

On first launch, rubot uses defaults (`api.openai.com`, `gpt-4o`). Inside the REPL, configure it:

```
/config set api_base_url https://api.openai.com/v1
/config set api_key sk-...
/config set model gpt-4o
```

Run `/config` to see all settings and the `.env` file path.

### Build from source

```bash
git clone https://github.com/null12138/rubot.git
cd rubot
cargo build --release
./target/release/rubot
```

## Configuration

All settings live in a **global** `.env` file — not the project directory:

| OS | Path |
|---|---|
| macOS / Linux | `~/.config/rubot/.env` |
| Windows | `%APPDATA%\rubot\.env` |

Override with `XDG_CONFIG_HOME` on Linux/macOS (the file lives under `$XDG_CONFIG_HOME/rubot/.env`).

### Config keys

| Variable | Default | Description |
|---|---|---|
| `RUBOT_API_BASE_URL` | `https://api.openai.com/v1` | OpenAI-compatible endpoint |
| `RUBOT_API_KEY` | `sk-placeholder` | API key |
| `RUBOT_MODEL` | `gpt-4o` | Heavy model (first turn, plans) |
| `RUBOT_FAST_MODEL` | = `RUBOT_MODEL` | Fast model (follow-up turns with tools) |
| `RUBOT_TAVILY_API_KEY` | empty | Tavily key for `web_search` (falls back to Bing) |
| `RUBOT_WORKSPACE` | `workspace` | Workspace directory (absolute or relative to config dir) |
| `RUBOT_MAX_RETRIES` | `3` | Max retries for LLM calls |
| `RUBOT_CODE_EXEC_TIMEOUT` | `30` | `code_exec` timeout in seconds |
| `RUBOT_WECHAT_BOT_TOKEN` | empty | WeChat bot token (run `rubot wechat setup`) |
| `RUBOT_WECHAT_BASE_URL` | `https://ilinkai.weixin.qq.com` | WeChat iLink API base |
| `RUBOT_ORKEY` | empty | OpenRouter API key for free sleep/dream model |
| `RUBOT_SLEEP_INTERVAL` | `300` | Idle seconds before sleep consolidation kicks in |

Use `/config set <key> <value>` inside rubot to update settings — changes take effect immediately.

### Local models

```bash
# Ollama
RUBOT_API_BASE_URL=http://localhost:11434/v1
RUBOT_API_KEY=ollama
RUBOT_MODEL=llama3

# vLLM
RUBOT_API_BASE_URL=http://localhost:8000/v1
RUBOT_API_KEY=vllm
RUBOT_MODEL=meta-llama/Meta-Llama-3-8B-Instruct
```

## Workspace

On first run rubot creates the workspace directory (`workspace/` by default):

```
workspace/
├── files/          Generated files, file_ops default base
├── tools/          MD-backed tool definitions (drop .md files here)
└── memory/
    ├── working/    Short-term (last few hours)
    ├── episodic/   Medium-term (days)
    └── semantic/   Long-term (permanent)
```

- `code_exec` runs in the **directory where you launched rubot**, not `workspace/files/`. Generated files from both CWD and `workspace/files/` are reported after execution.
- `file_ops` bare paths (e.g. `foo.txt`) resolve to `workspace/files/`. Use `files/...`, `tools/...`, `memory/...` or absolute paths for other locations.

## Memory

Rubot has a three-tier Ebbinghaus memory system the agent actively uses during conversation.

```
workspace/memory/
├── working/       Short-term (hours). Auto-evicted if not reviewed.
├── episodic/      Medium-term (days-weeks). Promoted from working at strength ≥ 2.
├── semantic/      Long-term (permanent). Promoted from episodic at strength ≥ 4.
└── memory_index.md Auto-generated index
```

The agent has LLM-callable memory tools:
- `memory_search` — search before starting a task to recall past findings
- `memory_add` — store facts, preferences, or patterns in any layer
- `memory_touch` — review and strengthen a memory entry
- `memory_due` — list entries overdue for review (Ebbinghaus spacing)

Each review (read, touch, or re-add with the same summary) increments the entry's strength. The decay cycle (runs every ~10 turns + at shutdown) promotes strong entries and evicts stale working entries.

### Sleep / Dream mode

After idle time (`RUBOT_SLEEP_INTERVAL`, default 5 min), the agent enters sleep consolidation. A separate cheap LLM:

1. Reviews all working + episodic entries
2. Merges related memories into consolidated episodic entries
3. Archives important patterns, evicts trivia, touches valuable entries
4. Runs the standard decay cycle

If `RUBOT_ORKEY` is set, sleep mode uses OpenRouter's free models (`google/gemini-2.0-flash-exp:free`) — costing nothing. Otherwise it falls back to your configured fast model. Sleep runs in-process just before processing the next user message.

## WeChat channel

Rubot supports WeChat (个人微信) via the iLink bot API. Start it as a standalone process:

```bash
rubot wechat
```

On first run, you need a bot token. Two ways to get one:

**Option 1 — Setup inside the REPL:**

```
/wechat setup
```

This prints a QR code in the terminal. Scan it with WeChat (发现 → 扫一扫). The token is saved to `~/.config/rubot/.env` automatically.

**Option 2 — Manual:**

```python
python3 -c "
import urllib.request, json
BASE = 'https://ilinkai.weixin.qq.com'
h = {'iLink-App-Id': 'bot', 'iLink-App-ClientVersion': '131338'}
# 1. Get QR code
qr = json.loads(urllib.request.urlopen(urllib.request.Request(f'{BASE}/ilink/bot/get_bot_qrcode?bot_type=3', headers=h)).read())
print('QR URL:', qr.get('qrcode_img_content','')[:80]+'...')
# 2. Poll (run after scanning)
import time
for _ in range(60):
    time.sleep(2)
    r = json.loads(urllib.request.urlopen(urllib.request.Request(f'{BASE}/ilink/bot/get_qrcode_status?qrcode={qr[\"qrcode\"]}', headers=h)).read())
    t = r.get('bot_token','')
    if t: print('Token:', t); break
"
```

Then set the token: `/config set wechat_bot_token <token>`

After setup, run `rubot wechat` to start the bot. Files created by rubot tools are auto-delivered to WeChat.

## REPL commands

| Command | Action |
|---|---|
| `/quit` / `/exit` | Save session memory and exit |
| `/clear` | Clear the conversation |
| `/memory` | Show memory index |
| `/memory search <query>` | Search memory |
| `/memory delete <id>` | Delete an entry |
| `/model [name]` | Show or set the heavy model |
| `/config` | List all config and `.env` path |
| `/config get <key>` | Show one config value |
| `/config set <key> <value>` | Save and apply a config value |
| `/plan` | Show the last executed plan |
| `/loop <task>\|<stop>` | Auto-loop on a task |
| `/wechat` | WeChat setup instructions |
| `/wechat setup` | Scan QR code to log in |
| `/wechat status` | Show current WeChat config |

## Project layout

```
src/
├── main.rs             REPL + command dispatcher
├── agent/              Core agent (split from single agent.rs)
│   ├── mod.rs          Agent struct, core Think→Act loop, subagents
│   ├── plan.rs         Plan mode + auto-plan detection
│   ├── runtime.rs      Build runtime, prompt messages, tool definitions
│   ├── session.rs      Session persistence, history compression
│   ├── stall.rs        Stall detection, blocked sources, recovery
│   ├── utils.rs        Constants, helpers, tool definitions
│   └── tests.rs        Agent tests
├── channel/
│   └── mod.rs          WeChat iLink channel (QR login, poll, send)
├── config.rs           .env loading, validation, persistence
├── personality.rs      System prompt (OS-aware, safety rules)
├── memory.rs           Three-layer flat-file memory
├── planner.rs          Multi-step chain executor
├── subagent.rs         Background child-agent manager
├── markdown.rs         Terminal markdown rendering
├── llm/
│   ├── client.rs       OpenAI-compatible HTTP client
│   └── types.rs        Request/response types
└── tools/
    ├── mod.rs          Tool registry + MD-backed tool loading
    ├── browser.rs      Headless Chromium via CDP
    ├── code_exec.rs    Bash/Python execution
    ├── file_ops.rs     File read/write/list
    ├── latex_pdf.rs    LaTeX → PDF rendering
    ├── web_fetch.rs    Fetch and parse web pages
    └── web_search.rs   Web search (Tavily + Bing fallback)
```

## License

MIT
