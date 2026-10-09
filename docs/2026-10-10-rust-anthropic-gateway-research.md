# 调研：用 Rust 自写 Anthropic 协议本地网关（RobinsonAI 三腿路由）是否可行、有没有现成轮子

日期：2026-10-10
背景：想让 Claude Code 走单一本地入口，按模型名分流三条腿——`opus`→Claude 组（Anthropic 协议直通）、`sonnet`→GPT 组（OpenAI Responses 协议 `/v1/responses`）、`haiku`→Gemini 组（Anthropic 协议走 `/antigravity/v1/messages` 特殊路径）。前提事实（RobinsonAI 三组协议墙、各端点行为）已经过实探确认，本文不再重复验证。

## 结论先行

1. **前提要修正一半**：LiteLLM 以前确实是纯 Python；但从 2025 年底起官方宣传语改成了 "Rust core with Python SDK"，正在把请求/响应翻译层迁进 Rust（`litellm-rust` workspace）。用户记的"LiteLLM 核心是 Rust"在今天已经部分成立。
2. **但"只复用它的 Rust 核心"不可行**：官方文档没有提供可嵌入的库 API。只有两种用法——Python 代理里给单个模型加 `rust: true` 开关，或者一个独立的 `litellm-ai-gateway` 二进制（beta、功能远少于 Python 版、无预编译镜像、要自己编译 workspace）。且其 Rust 路径目前不支持 tool calls、图片、流式 chat completions，遇到就静默回退 Python。对这个场景它帮不上忙。
3. **Rust 生态里没有任何现成项目做 Anthropic → Responses API 翻译**——这是本场景唯一的真翻译活。所有 Rust 代理（anthropic-proxy、claude-code-mux、anthropic-proxy-rs、coproxy……）都只翻译到 OpenAI **chat completions**。而 GPT 组已实探确认只认 `/v1/responses`。这块是空的，必须自己写。
4. **推荐：自写，非流式起步**。理由：claude 腿和 gemini 腿是纯 passthrough（改 URL + 改 key），拿任何现成项目都大材小用；唯一的难点（Responses 翻译 + SSE 重编码）恰恰没有现成 Rust 轮子，用了项目还得自己补，不如整体自写。最小栈 axum + reqwest + serde_json + tokio，非流式版约 600–800 行，加上流式重编码约 1200–1500 行。
5. **动手前先花一分钟测一件事**（决策分叉点）：`curl` 试一下 GPT 组是否也放行 `/v1/chat/completions`。背景事实只验证了 `/v1/responses` 可用、`/v1/messages` 403，chat completions 未测。如果放行，`cargo install claude-code-mux` 或 `anthropic-proxy` 现成就能跑全三条腿，一行不用写。
6. **claude-code-router（Node）仍是"最省事"基线**：它的 `openai-responses` transformer 已内置（[issue #1061 确认](https://github.com/musistudio/claude-code-router/issues/1061)），一个进程理论上能吃三条腿；但有已知 bug [#1515](https://github.com/musistudio/claude-code-router/issues/1515)（thinking 参数被透传上游导致 GPT 组请求失败）。作为 Rust 自写版的对照组和兜底保留。

---

## 一、前提验证：LiteLLM 到底是不是 Rust

- **历史**：LiteLLM 长期是纯 Python 项目（SDK + Python FastAPI 代理）。
- **现状**：官方博客[宣布将 AI 网关迁移到 Rust](https://docs.litellm.ai/blog/litellm-rust-launch)（宣称 15x 吞吐、11x 更省内存），GitHub 仓库描述已是 "The fastest, litest AI Gateway. Rust core with Python SDK"，仓库里有 `litellm-rust/` 目录、`rust-toolchain.toml`（[github.com/BerriAI/litellm](https://github.com/BerriAI/litellm)）。
- **架构**（[Rust Gateway Beta 文档](https://docs.litellm.ai/docs/proxy/rust_gateway)）：
  - **用法 1（推荐）**：Python 代理照常跑，单个模型的 `litellm_params` 里加 `rust: true`，只有"协议翻译 + 出网"走 Rust，Rust 失败自动回退 Python。需要 v1.94.0+。
  - **用法 2**：独立二进制 `litellm-ai-gateway`（Axum server）整体替换 Python 宿主——但文档明说"覆盖的路由比 Python 宿主少，尚不具备完整代理功能集"，无预编译 Docker 镜像，要从源码开 `server` feature 编译。
- **没有库/嵌入 API**：文档只给出上面两种用法，没有"把翻译层当 crate 引进自己项目"的路径。
- **Rust 路径的功能面**：`/v1/messages`（Anthropic 入口）只对 `anthropic`、`azure_ai` 后端开了 Rust 路径；`/chat/completions` 只支持非流式纯文本对话。**tool calls、图片、JSON mode、extended thinking、prompt caching 等一律静默走回 Python**。

小结：对"我要一个自己编译的独立 Rust 小工具"这个需求，LiteLLM 的 Rust 部分既不能当库用，功能也不够，反而把 Python 运行时一起拖进来。**排除复用 LiteLLM。**

## 二、Rust 生态调查：有没有能直接用的

### 2.1 关键缺口

搜遍 crates.io / GitHub / lib.rs，**所有"收 Anthropic `/v1/messages`、翻译到 OpenAI"的 Rust 项目都只翻到 `/chat/completions`**，没有一个支持 `/v1/responses`。做 Anthropic↔Responses 翻译的只有：

- **LiteLLM（Python）**：[官方有完整的 /v1/messages → /responses 参数映射文档](https://docs.litellm.ai/docs/anthropic_unified/messages_to_responses_mapping)，翻译代码在 `litellm/llms/anthropic/experimental_pass_through/responses_adapters/transformation.py`——这是自写时最好的参照物。
- **xhd2015/llm-proxy（Go）**：[`anthropic2openai` 包](https://pkg.go.dev/github.com/xhd2015/llm-proxy/pkgs/anthropic2openai) 明确做了 Responses↔Messages 双向翻译，可以当第二参照。
- claude-code-router（Node）的 `openai-responses` transformer。

Rust 侧这块是真空。

### 2.2 候选项目对比表

| 项目 | 覆盖范围 | 服务端 /v1/messages? | SSE 流式? | tool calls? | 翻译目标 | 维护状态 | 能否当库复用? |
|---|---|---|---|---|---|---|---|
| [anthropic-proxy](https://lib.rs/crates/anthropic-proxy) (crates.io) | Anthropic→OpenAI 兼容端点 | 是 | 是 | 是（tool_choice 不支持，恒为 auto） | **chat completions** | v1.2.0，2026-05 更新，下载量极小（89） | 否，纯二进制（~4K SLoC） |
| [claude-code-mux](https://lib.rs/crates/claude-code-mux) (`ccm`) | 本地 Claude Code 多 provider 路由器 | 是（含 count_tokens 端点） | 是（零拷贝 SSE 透传） | 是 | **chat completions**（18+ provider，含 Gemini 原生） | v0.6.3，2025-11 更新 | 否，二进制（~5.5K SLoC），TOML 配置 + Web UI，支持按模型名/子代理标记/thinking 分流和 failover |
| [anthropic-proxy-rs](https://github.com/m0n0x41d/anthropic-proxy-rs) (GitHub) | Anthropic→OpenAI 兼容端点 | 是 | 是 | 是（tool_choice 同上） | **chat completions** | 93 stars，MIT，小项目 | 否，二进制 |
| [coproxy](https://crates.io/crates/cop roxy) | Anthropic→GitHub Copilot | 是 | 是 | 是 | Copilot 内部的 chat completions | 2026-05 更新 | 否，且后端不对口 |
| [async-openai](https://docs.rs/async-openai) (crates.io) | OpenAI **客户端** SDK | —（客户端库） | 是 | 是 | — | v0.42.1，2026-09 更新，900 万下载，**最有维护保障** | **是**，开 `responses` feature 即有完整 Responses 类型（自写 GPT 腿时直接用） |
| [tau-anthropic](https://crates.io/crates/tau-anthropic) | Anthropic 客户端类型库 | —（客户端库） | 是 | 是 | — | v0.7.0，2026-09 更新 | 是，可借用其 Anthropic 请求/SSE 事件类型定义 |
| [misanthropy](https://crates.io/crates/misanthropy) | Anthropic 客户端 | —（客户端库） | 是 | 部分 | — | 2025-06 后无更新 | 一般 |
| [anthropic_rust_sdk](https://github.com/ThreatFlux/anthropic_rust_sdk) | Anthropic 客户端 | —（客户端库） | 是 | 是 | — | 活跃 | 是，同上 |

要点：

- **能收 `/v1/messages` 的全是二进制应用，没有一个是库**；即使想 fork 改造，它们的翻译目标（chat completions）也不对，改造量≈重写 GPT 腿。
- **claude-code-mux 是功能上最接近的**（按模型名路由、Gemini 支持、failover、count_tokens），若 GPT 组实测放行 chat completions，它是零成本方案。
- **自写时真正值得当依赖的是 `async-openai`**（Responses 类型齐全、维护最勤），Anthropic 侧类型可以抄 `tau-anthropic` 或干脆手写最小结构体。

## 三、自写复杂度估计

### 3.1 最小技术栈

```
axum（服务端，自带 SSE body 支持）
reqwest（出网客户端，stream feature）
serde_json（JSON；Anthropic 侧类型可以只解析需要的字段，其余 Value 透传）
tokio + futures（Stream 处理）
```

SSE 解析不必引入 eventsource-stream：上游 SSE 每条事件就是 `event: xxx\ndata: {...}\n\n`，手写按行切分足够。

### 3.2 三条腿各自的活

| 腿 | 工作性质 | 估计代码量 |
|---|---|---|
| claude（opus） | 纯反向代理透传：收什么转发什么，response body 原样流回 | ~100–150 行 |
| gemini（haiku） | 同上，仅改写 URL（加 `/antigravity` 前缀）和 `x-api-key` | ~30–50 行（与 claude 腿共用 handler） |
| gpt（sonnet）非流式 | 请求映射 + 响应映射（见 3.3） | ~300–500 行 |
| gpt 流式 | Anthropic SSE 事件序列 ↔ Responses SSE 事件序列重编码（见 3.3） | ~300–500 行 |

合计：**非流式优先版 ~600–800 行，完整含流式 ~1200–1500 行**。对熟手是 1–2 天（非流式）+ 1 天（流式调试）的量。

### 3.3 真正的难点（全部集中在 GPT 腿）

照 [LiteLLM 的官方映射文档](https://docs.litellm.ai/docs/anthropic_unified/messages_to_responses_mapping)逐条列：

1. **tool_use/tool_result 是结构性变换不是改名**：Anthropic 把工具调用/结果嵌在 `messages[].content` 块里；Responses API 要求把它们**提升（hoist）成顶层 input items**——`tool_result`→`{"type":"function_call_output","call_id":...,"output":...}`，`tool_use`→`{"type":"function_call","call_id":...,"name":...,"arguments":"<JSON字符串>"}`。反向同理。
2. **字段更名与语义差**：`system`→`instructions`（数组时只留 text 块拼接，非文本块静默丢弃）；`max_tokens`→`max_output_tokens`；`tool_choice`: `any`→`required`、`tool`→`{"type":"function",...}`。
3. **stop_reason 映射**（响应侧）：输出含 `function_call`→`tool_use`；`status=="incomplete"`→`max_tokens`（优先级更高）；否则 `end_turn`。
4. **thinking→reasoning**：`budget_tokens` 阈值映射 effort（≥10000→high、≥5000→medium、≥2000→low、否则 minimal）；出向时 reasoning summary 变回 thinking 块或退化为纯文本。
5. **静默丢失**：`stop_sequences`、`top_k` 在这条路上没有对应物，直接丢（Claude Code 发的请求里 `stop_sequences` 通常为空，实际影响小，但要心里有数）。
6. **流式重编码是最难的一块**：要把 Responses 的事件（`response.output_item.added`、`response.output_text.delta`、function_call 的参数增量、`response.completed`）重新编排成 Anthropic 客户端期待的严格序列（`message_start` → `content_block_start`/`content_block_delta`/`content_block_stop`（可多块交错）→ `message_delta`（带 stop_reason + usage）→ `message_stop`）。**建议第一版 GPT 腿先只做非流式**（收流式请求、上游非流式拿全量、一次性编成 SSE 事件序列发回），Claude Code 完全能接受；真流式后补。
7. **杂项**：`/v1/messages/count_tokens` 端点 Claude Code 会调（可 stub 成粗估或透传给 claude 腿）；`metadata.user_id`→`user`（截 64 字符）+ `prompt_cache_key`。

claude/gemini 两条腿没有任何协议难点，唯一注意 reqwest 透传时保留原始 header（除 host/authorization）和流式 body（`bytes_stream()` 直通 axum 的 `Body::from_stream`）。

## 四、最终建议

**自写，但按这个顺序走：**

1. **第 0 步（5 分钟，决定一切）**：拿 GPT 组 key `curl` 测 `/v1/chat/completions`。
   - 放行 → 直接 `cargo install claude-code-mux`（或 `anthropic-proxy`），配 TOML 分流，收工，一行代码不用写。
   - 只认 `/v1/responses`（按背景事实大概率如此）→ 走自写路线，没有现成轮子可捡。
2. **自写版骨架**：单 binary crate，`axum` + `reqwest` + `serde_json` + `tokio`；一个 handler 按请求里的 `model` 前缀分三路；claude/gemini 两路纯透传；gpt 路先用 `async-openai` 的 `responses` 类型做非流式翻译。**第一版不做真流式**（伪流式：上游全量后一次性编 SSE）。跑通日常 Claude Code 使用后，再补 GPT 腿真流式重编码。
3. **参照物**：翻译映射逐条照抄 [LiteLLM 的 messages→responses 映射文档](https://docs.litellm.ai/docs/anthropic_unified/messages_to_responses_mapping)（它就是这份映射的权威实现方）；卡住时看 [xhd2015/llm-proxy 的 anthropic2openai（Go）](https://pkg.go.dev/github.com/xhd2015/llm-proxy/pkgs/anthropic2openai)当第二参考。
4. **claude-code-router 保留为兜底**：若自写流式调试卡壳，临时用 CCR 的 `openai-responses` transformer 顶着（注意避坑 [#1515 thinking 参数透传 bug](https://github.com/musistudio/claude-code-router/issues/1515)）。

一句话：**LiteLLM 的 Rust 核心（部分存在但不可复用）；Rust 生态没有 Responses 翻译轮子；这个场景的最优解是自己写 ~1 千行的 axum 小网关——除非 chat completions 测试通过，那样 claude-code-mux 直接白嫖。**

## 引用来源

- https://github.com/BerriAI/litellm
- https://docs.litellm.ai/blog/litellm-rust-launch
- https://docs.litellm.ai/docs/proxy/rust_gateway
- https://docs.litellm.ai/docs/anthropic_unified/messages_to_responses_mapping
- https://lib.rs/crates/anthropic-proxy
- https://lib.rs/crates/claude-code-mux
- https://github.com/m0n0x41d/anthropic-proxy-rs
- https://crates.io/crates/tau-anthropic
- https://docs.rs/async-openai
- https://pkg.go.dev/github.com/xhd2015/llm-proxy/pkgs/anthropic2openai
- https://github.com/musistudio/claude-code-router/issues/1061
- https://github.com/musistudio/claude-code-router/issues/1515
