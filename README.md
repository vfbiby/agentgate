# claude-anygate

A local Anthropic-protocol gateway that lets **Claude Code** (and any Anthropic-API client)
talk to *any* model behind *any* OpenAI- or Anthropic-compatible relay — with per-tier
model mixing in a single endpoint.

Fork of [9j/claude-code-mux](https://github.com/9j/claude-code-mux) v0.6.3 (upstream archived,
MIT). This fork carries three fixes/completions that upstream never shipped:

## What this fork changes

| Area | Change |
|---|---|
| `src/providers/sse_bridge.rs` (**new**, ~660 lines) | **OpenAI SSE → Anthropic SSE streaming translation.** Upstream passed raw `chat.completion.chunk` bytes through with a `TODO`. Now emits a proper Anthropic event sequence (`message_start` → `content_block_delta`(text_delta / input_json_delta) → `message_delta`(stop_reason mapping, usage) → `message_stop`), including tool-use streaming. Also provides a stateful SSE re-framer (handles events split across TCP chunks). |
| `src/server/mod.rs` | ① The direct-lookup branch of `POST /v1/messages` now supports streaming (previously it force-called non-streaming `send_message` and choked on `stream:true`). ② SSE responses are re-framed safely — axum's `Event::data` panics on multi-line payloads. ③ New `GET /v1/models` endpoint for gateway model discovery (Claude Code `/model` shows real backend models). |
| `src/router/mod.rs` | Auto-map (e.g. `^claude-` → default model) now **skips model names that are exactly registered by an enabled provider** — otherwise the models listed by `/v1/models` are unreachable. |
| `src/providers/registry.rs` | `model_to_provider` map is now actually populated in `from_configs` (upstream left it write-only; `list_models()` always returned empty). |

## Quick start

```bash
cargo build --release
./target/release/ccm -c /path/to/ccm.toml start
```

Example `ccm.toml` — one endpoint, three backend groups mixed per Claude Code tier:

```toml
[server]
port = 13456
host = "127.0.0.1"

[[providers]]
name = "rb-claude"
provider_type = "anthropic"            # Anthropic /v1/messages passthrough
base_url = "https://your-relay.example"
api_key = "sk-..."
models = ["claude-opus-5-5"]

[[providers]]
name = "rb-gpt"
provider_type = "openai"               # OpenAI chat completions, translated
base_url = "https://your-relay.example/v1"
api_key = "sk-..."
models = ["gpt-6.1-sol"]

[[providers]]
name = "rb-gemini"
provider_type = "anthropic"            # another Anthropic-compatible path
base_url = "https://your-relay.example/antigravity"
api_key = "sk-..."
models = ["gemini-3.8-flash-low", "gemini-3.8-flash-medium", "gemini-3.8-flash-high"]

[router]
default = "gpt-6.1-sol"                # sonnet tier
background = "gemini-3.8-flash-low"    # haiku tier
think = "claude-opus-5-5"              # plan/thinking mode
```

Point your client at it:

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:13456"
export ANTHROPIC_AUTH_TOKEN="anything"
claude
```

Notes:
- Provider config uses **`base_url`** (not `api_base_url` as in some upstream examples —
  unknown fields are silently ignored and you silently hit the provider defaults).
- Routing order: websearch > subagent tag > thinking/plan > background > auto-map > default.
  Names exactly registered by a provider are never auto-mapped.
- A web admin UI is served at the same port (`/`).

## Docs

- [docs/gateway-model-discovery-filters.md](docs/gateway-model-discovery-filters.md) —
  Claude Code `/model` 网关模型发现的三层过滤规则（逆向自 v2.1.293，含对策）
- [docs/zsh-integration.md](docs/zsh-integration.md) —
  zsh 函数接入模板：环境隔离、防变量泄漏清单、多网关共存

## License

MIT, same as upstream. Upstream © 2025 9j. Changes in this fork © 2026 contributors.
