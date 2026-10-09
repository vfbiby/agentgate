# 用 zsh 函数接入网关（多网关共存的组织方式）

本项目推荐的使用方式不是改 `~/.claude/settings.json`，而是在 `~/.zshrc` 里为每个
网关写一个独立函数：**环境变量隔离 + 独立配置目录**，多个中转站/网关互不干扰、
官方登录原样保留。

## 核心原则

1. **每个网关一个函数、一个独立 `CLAUDE_CONFIG_DIR`**
   目录里 `settings.json` / `.claude.json` 独立，其余子项符号链接回 `~/.claude`
   共享（skills、plugins、hooks、历史等都不丢）：

   ```bash
   mkdir -p ~/.claude_rbmux && cd ~/.claude_rbmux
   for d in agent-memory agents backups cache file-history history.jsonl hooks \
            jobs paste-cache plans plugins projects session-env sessions \
            shell-snapshots skills tasks telemetry; do
     ln -sfn "$HOME/.claude/$d" "$d"
   done
   echo '{"hasCompletedOnboarding": true}' > .claude.json
   ```

2. **settings.json 里不要写 `availableModels`**（会触发 Claude Code 的模型家族
   校验，把非 Claude 家族的网关模型全过滤掉，见
   [gateway-model-discovery-filters.md](gateway-model-discovery-filters.md) 第 3 层）。

3. **清除上游环境泄漏**：如果你同时用其它中转站（它们常设 `ANTHROPIC_MODEL` 等），
   函数里必须用 `env -u` 把这些变量清干净，否则「selected model 不存在」类报错
   就是它们串台造成的。

## 网关型函数模板（本项目）

```zsh
# <网关名> — use via `robmux` command
robmux() {
  # 首次调用自动拉起路由服务（已运行则跳过）
  if ! pgrep -f "claude_rbmux/ccm.toml" >/dev/null 2>&1; then
    http_proxy="http://127.0.0.1:7890" \      # 路由服务出网走代理（按需）
    https_proxy="http://127.0.0.1:7890" \
    no_proxy="localhost,127.0.0.1" \
    NO_PROXY="localhost,127.0.0.1" \
    nohup "$HOME/.cargo/bin/ccm" -c "$HOME/.claude_rbmux/ccm.toml" start \
      > /tmp/ccm.log 2>&1 &
    sleep 1
  fi
  env \
    -u ANTHROPIC_API_KEY \
    -u ANTHROPIC_MODEL \
    -u ANTHROPIC_DEFAULT_OPUS_MODEL \
    -u ANTHROPIC_DEFAULT_SONNET_MODEL \
    -u ANTHROPIC_DEFAULT_HAIKU_MODEL \
    -u CLAUDE_CODE_SUBAGENT_MODEL \
    -u ANTHROPIC_SMALL_FAST_MODEL \
    CLAUDE_CONFIG_DIR="$HOME/.claude_rbmux" \
    no_proxy="localhost,127.0.0.1" \
    NO_PROXY="localhost,127.0.0.1" \
    ANTHROPIC_BASE_URL="http://127.0.0.1:13456" \
    ANTHROPIC_AUTH_TOKEN="mux-local" \
    CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY="1" \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1" \
    claude "$@"
}
```

要点：

- **`-u` 清单是硬要求**：`ANTHROPIC_MODEL`、`ANTHROPIC_DEFAULT_*_MODEL`、
  `CLAUDE_CODE_SUBAGENT_MODEL` 等一旦从其它会话/函数泄漏进来，Claude Code 会拿
  别家的模型名请求你的网关，报「selected model 不存在」。
- **`no_proxy` 必须含 localhost**：`ANTHROPIC_BASE_URL` 指向 127.0.0.1，若终端里
  有全局代理变量而没有这条例外，请求会被塞给代理。
- **`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`** 才会拉取 `/v1/models`
  （`/model` 里显示全部后端模型的前提）。
- 改完函数要**开新终端**或 `source ~/.zshrc` 才生效。

## 简单中转站函数模板（不经本项目，直连 Anthropic 兼容网关）

```zsh
robclaude() {
  env \
    -u ANTHROPIC_API_KEY \
    -u ANTHROPIC_MODEL \
    -u ANTHROPIC_DEFAULT_OPUS_MODEL \
    -u ANTHROPIC_DEFAULT_SONNET_MODEL \
    -u ANTHROPIC_DEFAULT_HAIKU_MODEL \
    -u CLAUDE_CODE_SUBAGENT_MODEL \
    -u ANTHROPIC_SMALL_FAST_MODEL \
    CLAUDE_CONFIG_DIR="$HOME/.claude_rbcc" \
    ANTHROPIC_BASE_URL="https://api.example-relay.com" \
    ANTHROPIC_AUTH_TOKEN="sk-..." \
    CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY="1" \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1" \
    claude "$@"
}
```

- 用 `ANTHROPIC_AUTH_TOKEN`，不用 `ANTHROPIC_API_KEY`（后者会触发 OAuth 弹窗）。
- 分组没有 haiku 档时，把 `ANTHROPIC_DEFAULT_HAIKU_MODEL` 映射到该分组里最便宜的
  模型（Claude Code 的后台任务会用到 haiku 档）。
- 该配置目录的 `settings.json` 同样不要写 `availableModels`。

## 两个网关函数并存时的档位归属

每个函数有自己的 `CLAUDE_CONFIG_DIR`，档位映射（`/model` 里的选择）也是按配置
目录独立保存的，互不影响。想换某档位的后端模型：改网关配置（本项目用 admin UI
的 Tier Mapping 页），不改函数。
