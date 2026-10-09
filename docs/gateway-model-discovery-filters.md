# Claude Code 网关模型发现的过滤规则

本文记录 Claude Code（实测版本 v2.1.293）从网关拉取模型列表到 `/model` 选择器展示之间
的**三层过滤**。想让自己的模型出现在 `/model` 里，必须同时通过这三层。结论来自对本机
Claude Code 二进制的逆向（strings + 上下文提取），已在本项目实测验证。

## 发现流程总览

会话启动时（异步），Claude Code 对 `ANTHROPIC_BASE_URL` 网关：

```
GET {BASE_URL}/v1/models?limit=1000
Authorization: Bearer <ANTHROPIC_AUTH_TOKEN>   （或 x-api-key）
anthropic-version: 2023-06-01
```

要求：设置了 `ANTHROPIC_AUTH_TOKEN`（或 apiKeyHelper / API key）、base URL 非
Anthropic 一方主机、未设 `ANTHROPIC_BASE_URL` 为一方主机。任一不满足则整个发现被跳过。

结果写入磁盘缓存（macOS：`~/.claude/cache/gateway-models.json`），**选择器读的是缓存**，
所以发现是异步的——改了网关模型列表后需要重开会话（或等缓存刷新）才能看到。

## 三层过滤

### 第 1 层：发现层 —— id 必须包含 "claude" 或 "anthropic"

```js
models.filter(m => /(claude|anthropic)/i.test(m.id))
```

id 里不含这两个词的模型（如裸的 `gpt-6.1-sol`、`gemini-3.8-flash-low`）进不了缓存。

**对策**：给非 Claude 模型造 `claude-` 前缀的插槽 id（如 `claude-gpt-6.1-sol`），
真名放进 `display_name`。

### 第 2 层：字符集层 —— id 只允许 `[a-z0-9-]`

选择器只显示匹配 `^[a-z0-9-]+$` 的 id，**点号不合法**。Claude 自家 id 从不用点号
（4.8 写作 `claude-opus-4-8`），所以 `claude-gpt-6.1-sol` 这种带点的插槽名会整条消失，
连 "From gateway" 注释都没有。

**对策**：插槽 id 净化——`[a-z0-9-]` 以外的字符全部替换为连字符并小写
（`gpt-6.1-sol` → 插槽 `claude-gpt-6-1-sol`）。本项目已内置此净化。

注意：`display_name` 字段**会被无视**——选择器显示的名字是它自己从 id 美化出来的
（`claude-opus-4-8` → 「Opus 4.8」）。所以真名要直接编进 id 里。

### 第 3 层：availableModels 层 —— 触发家族校验（最容易踩）

settings.json 里**只要存在 `availableModels` 键**，`/model` 的每个选项都要过一遍
模型目录校验：解析出的模型家族必须是 **opus / sonnet / haiku / fable** 之一，否则丢弃。

- `claude-opus-5` → opus 家族 ✅
- `claude-gpt-6-1-sol` → gpt 家族 ❌ 整条丢弃（连"From gateway"注释都救不回来）

**对策**：网关型配置的 settings.json **不要写 `availableModels`**。该键只适合
「纯官方 Claude 模型」的场景。

## 另一条通道：Custom model（环境变量档位）

`ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU,FABLE}_MODEL` 环境变量
会在选择器生成「Custom X model (名字)」条目——**不过滤**，任何名字都能显示
（如 `glm-5.3-flash[1m]`）。这条通道和网关发现相互独立。

可用的高级变量（v2.1.293 起在 `ANTHROPIC_DEFAULT_SONNET_MODEL` 之外）：

| 变量 | 作用 |
|---|---|
| `ANTHROPIC_DEFAULT_SONNET_MODEL_NAME` | 自定义该档位行的**标签**（左边粗体名） |
| `ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION` | 自定义该档位行的**描述文字** |

（Opus/Haiku/Fable 同构。）代价：设置后该档位按钮直接发送映射的名字。

## 1M 上下文声明：`[1m]` 尾缀

Claude Code 声明 1M 上下文的官方方式是在模型名后加 `[1m]`
（对目录外模型的警告原文：*append [1m] to the model name for 1M*）：

- 带尾缀 → 按 1M 处理（/context、自动压缩阈值），Claude Code 发请求前会自己剥掉尾缀
- 不带 → 对目录外模型假设 200k，并在该线触发自动压缩
  （可用 `CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1` 关闭强制压缩，
  退回「等 API 自己报错」的旧行为）

对本项目的影响：上游中转站**不认**带 `[1m]` 的模型名，所以本 fork 的路由入口会先
剥掉 `[1m]` 尾缀再做插槽解析/转发——网关配置和 admin UI 的 modelPicker 行可以放心
使用带 `[1m]` 的名字声明 1M。注意 `[1m]` 只应加在真支持 1M 的模型上（给 272k 的
模型声明 1M 会让自动压缩失去保护意义）。

## modelPicker：完全自定义 /model 列表

settings.json 支持 `modelPicker`（v2.1.293 逆向确认的行字段）：

```json
"modelPicker": {
  "replaceBuiltInOptions": true,
  "options": [
    { "model": "claude-gemini-3-8-flash-medium[1m]",
      "label": "gemini-3.8-flash-medium",
      "behavesAs": "claude-sonnet-5-5" }
  ]
}
```

- `model`：发送给网关的 id，允许带 `[1m]` 尾缀
- `label` / `description`：自定义该行显示（覆盖 id 美化）
- `behavesAs`：把目录外 id 映射到目录内已知模型——消除「isn't described by model
  catalog」警告，并继承其目录行为
- `replaceBuiltInOptions: true`：`/model` 只显示 Default + 这些行（内置档位、
  网关发现列表全部隐藏）
- 本项目 admin UI 的 Visibility/Tier Mapping 与之配合：Visibility 管
  `/v1/models`（API 层可见性），modelPicker 管选择器显示层

## 本项目的组合拳

1. `/v1/models` 对非 claude 模型返回净化插槽 id + 真名 display_name（过第 1、2 层）
2. 路由器收到 `claude-` 前缀插槽名时，剥前缀做归一化匹配（连字符/点号等价），
   还原为注册模型转发
3. settings.json 不放 `availableModels`（避开第 3 层）
4. 档位按钮的指向由 admin UI「Tier Mapping」页控制（[[models]] 映射 + router.background）
