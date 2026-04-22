# Rubot

Minimal autonomous AI agent in Rust: LLM + core tools + flat-file memory. A Think→Act loop that runs in a terminal REPL.

## Features

- **LLM-agnostic** — works with any OpenAI-compatible API (OpenAI, Azure, Ollama, vLLM, LM Studio, etc.)
- **Five built-in tools** — `web_search`, `web_fetch`, `code_exec`, `file_ops`, `latex_pdf`
- **MD-backed tools** — drop a `.md` file in `workspace/tools/` to add new tools at runtime
- **Flat-file memory** — three-layer Ebbinghaus-style memory (working / episodic / semantic)
- **Multi-step plans** — LLM can emit a JSON plan that executes sequentially
- **Loop mode** — drive a single task with a stop condition
- **Heavy/fast model split** — first turn uses the heavy model, follow-ups use the fast model

## Installation

### Pre-built binaries (recommended)

Download the latest release for your platform from [GitHub Releases](https://github.com/opener/rubot/releases/latest).

| Platform | File |
|---|---|
| Linux x86_64 | `rubot-linux-amd64.tar.gz` |
| Linux ARM64 | `rubot-linux-arm64.tar.gz` |
| Linux ARMv7 | `rubot-linux-armhf.tar.gz` |
| Linux x86_64 (static) | `rubot-linux-amd64-musl.tar.gz` |
| macOS Apple Silicon | `rubot-macos-arm64.tar.gz` |
| macOS Intel | `rubot-macos-amd64.tar.gz` |
| Windows x86_64 | `rubot-windows-amd64.zip` |

**Linux / macOS:**

```bash
tar xzf rubot-linux-amd64.tar.gz
sudo mv rubot /usr/local/bin/
```

**Windows:**

Extract `rubot.exe` from the zip and place it somewhere on your PATH.

### One-line install

**Linux / macOS:**

```bash
curl -fsSL https://raw.githubusercontent.com/opener/rubot/main/install.sh | bash
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/opener/rubot/main/install.ps1 | iex
```

### Build from source

```bash
git clone https://github.com/opener/rubot.git
cd rubot
cargo build --release
./target/release/rubot
```

## Prerequisites

| | Shell | Python |
|---|---|---|
| **Linux** | bash (preinstalled) | `sudo apt-get install python3` |
| **macOS** | bash (preinstalled) | `brew install python3` or Xcode CLI tools |
| **Windows** | PowerShell (preinstalled) | [python.org](https://python.org) or `winget install Python.Python.3` |

> On Windows, `lang: "bash"` in `code_exec` runs PowerShell. `lang: "python"` uses `python` (not `python3`).

## Configuration

All settings via environment variables (`.env` file in the working directory):

| Variable | Default | Description |
|---|---|---|
| `RUBOT_API_BASE_URL` | `https://api.openai.com/v1` | OpenAI-compatible endpoint |
| `RUBOT_API_KEY` | `sk-placeholder` | API key |
| `RUBOT_MODEL` | `gpt-4o` | Heavy model name |
| `RUBOT_FAST_MODEL` | = `RUBOT_MODEL` | Fast model for follow-up turns |
| `RUBOT_WORKSPACE` | `workspace` | Workspace directory (relative or absolute) |
| `RUBOT_MAX_RETRIES` | `3` | Max retries for LLM calls |
| `RUBOT_CODE_EXEC_TIMEOUT` | `30` | `code_exec` timeout (seconds) |

### Quick setup

```bash
cp .env.example .env
# Edit .env with your API key and endpoint
$EDITOR .env
rubot
```

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

On first run, rubot creates a `workspace/` directory structure:

```
workspace/
├── files/          code_exec sandbox — generated files go here
├── tools/          MD-backed tool definitions (drop .md files here)
└── memory/
    ├── working/    short-term (last few hours)
    ├── episodic/   medium-term (days)
    └── semantic/   long-term (permanent)
```

Set `RUBOT_WORKSPACE` to an absolute path to customize the location.

`file_ops` uses `workspace/files/` as the default base for bare relative paths like `foo.txt`, but you can also target `tools/...`, `memory/...`, `files/...`, or any absolute path that stays inside the configured workspace root.

## REPL commands

| Command | Action |
|---|---|
| `/quit` / `/exit` | save session memory and exit |
| `/clear` | clear terminal |
| `/memory` | list memory index |
| `/memory <id>` | show a memory entry |
| `/memory search <query>` | keyword search |
| `/memory delete <id>` | delete an entry |
| `/memory clear` | wipe all memories |
| `/model [name]` | show or set the heavy model |
| `/config` | list effective config and `.env` path |
| `/config get <key>` | show one config value |
| `/config set <key> <value>` | save config to `.env` and apply it |
| `/plan` | show the last executed plan |
| `/loop <task>\|<stop>` | auto-loop on a task until stop condition |

## Project layout

```
src/
├── main.rs          REPL + command dispatcher
├── agent.rs         Think→Act loop
├── config.rs        env config
├── personality.rs   system prompt (OS-aware)
├── memory.rs        three-layer flat-file memory
├── planner.rs       multi-step chain executor
├── markdown.rs      terminal markdown rendering
├── llm/             OpenAI-compatible client + types
└── tools/           registry + built-in tools + MD-backed tools
```

## Cross-compilation

The CI builds 7 targets via GitHub Actions. To cross-compile locally:

```bash
# Install cross
cargo install cross --version 0.2.5

# Build for ARM64 Linux
cross build --release --target aarch64-unknown-linux-gnu
```

See `Cross.toml` for Docker image configuration.

## License

MIT
