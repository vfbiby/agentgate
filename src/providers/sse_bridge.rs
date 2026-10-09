//! SSE bridge utilities.
//!
//! Two pieces live here:
//!
//! 1. [`SseFrameBuffer`] / [`SseReframer`] — a stateful accumulator that turns
//!    raw upstream byte chunks into complete [`SseEvent`]s (handling events
//!    split across TCP chunks). Used to safely re-frame provider SSE into
//!    axum `Event`s: axum panics when `Event::data` receives a string that
//!    contains newlines, so raw passthrough of multi-line SSE frames is not
//!    possible.
//!
//! 2. [`OpenAiSseTransformer`] — converts an upstream OpenAI
//!    `chat.completions` SSE byte stream into an Anthropic Messages SSE byte
//!    stream (`event: <type>\ndata: <json>\n\n` framing, one complete
//!    Anthropic event per stream item).

use crate::models::{AnthropicRequest, ContentBlock, MessageContent, SystemPrompt};
use crate::providers::error::ProviderError;
use crate::providers::streaming::{parse_sse_events, SseEvent};
use bytes::Bytes;
use futures::stream::Stream;
use pin_project::pin_project;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Stateful SSE framing
// ---------------------------------------------------------------------------

/// Accumulates raw text and yields complete SSE events.
pub struct SseFrameBuffer {
    buf: String,
    finished: bool,
}

impl SseFrameBuffer {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            finished: false,
        }
    }

    /// Feed raw text; returns every event that became complete.
    pub fn push(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.finished {
            return events;
        }
        // Normalize CRLF so frame detection only needs to look for "\n\n".
        self.buf.push_str(&text.replace("\r\n", "\n"));
        while let Some(end) = self.buf.find("\n\n") {
            let frame: String = self.buf.drain(..end + 2).collect();
            events.extend(parse_sse_events(&frame));
        }
        events
    }

    /// Flush any buffered partial frame (call once when the stream ends).
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.buf.trim().is_empty() {
            self.buf.clear();
            return Vec::new();
        }
        let frame = std::mem::take(&mut self.buf);
        parse_sse_events(&frame)
    }
}

impl Default for SseFrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Stream adapter: raw provider byte chunks -> complete SSE events.
#[pin_project]
pub struct SseReframer<S, E> {
    #[pin]
    inner: S,
    buf: SseFrameBuffer,
    pending: VecDeque<SseEvent>,
    flushed: bool,
    _error: std::marker::PhantomData<E>,
}

impl<S, E> SseReframer<S, E> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buf: SseFrameBuffer::new(),
            pending: VecDeque::new(),
            flushed: false,
            _error: std::marker::PhantomData,
        }
    }
}

impl<S, E> Stream for SseReframer<S, E>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: Into<ProviderError>,
{
    type Item = Result<SseEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            if let Some(ev) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(ev)));
            }
            if *this.flushed {
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    this.pending.extend(this.buf.push(&text));
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Ready(None) => {
                    this.pending.extend(this.buf.finish());
                    *this.flushed = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI chat.completions -> Anthropic Messages streaming transform
// ---------------------------------------------------------------------------

/// Rough token estimate (chars / 4), mirroring the CountTokens fallback.
pub fn estimate_input_tokens(request: &AnthropicRequest) -> u32 {
    let mut total_chars = 0usize;

    if let Some(ref system) = request.system {
        match system {
            SystemPrompt::Text(text) => total_chars += text.len(),
            SystemPrompt::Blocks(blocks) => {
                for block in blocks {
                    total_chars += block.text.len();
                }
            }
        }
    }

    for msg in &request.messages {
        match &msg.content {
            MessageContent::Text(text) => total_chars += text.len(),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => total_chars += text.len(),
                        ContentBlock::ToolResult { content, .. } => {
                            total_chars += content.to_string().len();
                        }
                        ContentBlock::Thinking { thinking, .. } => total_chars += thinking.len(),
                        _ => {}
                    }
                }
            }
        }
    }

    (total_chars / 4) as u32 + 1
}

#[derive(Debug)]
struct ToolBlockState {
    /// Anthropic content block index.
    anthropic_index: usize,
    /// Whether any partial_json delta has been emitted.
    args_emitted: bool,
}

/// Tracks translation state for one streaming response.
struct TransformState {
    model: String,
    message_id: String,
    estimated_input_tokens: u32,
    message_started: bool,
    next_index: usize,
    /// Currently open text block (Anthropic index), if any.
    open_text_block: Option<usize>,
    /// OpenAI tool_call index -> block state.
    tool_blocks: HashMap<u64, ToolBlockState>,
    finish_reason: Option<String>,
    usage_input_tokens: Option<u64>,
    usage_output_tokens: Option<u64>,
    /// Serialized Anthropic events awaiting emission.
    out: VecDeque<Bytes>,
    done: bool,
}

impl TransformState {
    fn new(model: String, estimated_input_tokens: u32) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            model,
            message_id: format!("msg_{:x}", nanos),
            estimated_input_tokens,
            message_started: false,
            next_index: 0,
            open_text_block: None,
            tool_blocks: HashMap::new(),
            finish_reason: None,
            usage_input_tokens: None,
            usage_output_tokens: None,
            out: VecDeque::new(),
            done: false,
        }
    }

    fn frame(&mut self, event_type: &str, payload: Value) {
        let text = format!("event: {}\ndata: {}\n\n", event_type, payload);
        self.out.push_back(Bytes::from(text));
    }

    fn ensure_message_start(&mut self) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        let payload = json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.estimated_input_tokens,
                    "output_tokens": 0,
                },
            },
        });
        self.frame("message_start", payload);
    }

    fn open_text_block(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        let payload = json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""},
        });
        self.frame("content_block_start", payload);
        self.open_text_block = Some(index);
        index
    }

    fn close_text_block(&mut self) {
        if let Some(index) = self.open_text_block.take() {
            let payload = json!({"type": "content_block_stop", "index": index});
            self.frame("content_block_stop", payload);
        }
    }

    fn handle_text_delta(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.ensure_message_start();
        // Re-opening after a tool block is fine: Anthropic allows multiple
        // text blocks, and Claude Code concatenates them.
        if self.open_text_block.is_none() {
            self.open_text_block();
        }
        let index = self.open_text_block.unwrap();
        let payload = json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text},
        });
        self.frame("content_block_delta", payload);
    }

    fn handle_tool_delta(&mut self, tool_call: &Value) {
        self.ensure_message_start();
        // Any open text block must be closed before tool blocks start.
        self.close_text_block();

        let openai_index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0);
        if !self.tool_blocks.contains_key(&openai_index) {
            let anthropic_index = self.next_index;
            self.next_index += 1;
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("toolu_{:x}{:02x}", nanos, openai_index));
            let name = tool_call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let payload = json!({
                "type": "content_block_start",
                "index": anthropic_index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
            });
            self.frame("content_block_start", payload);
            self.tool_blocks.insert(
                openai_index,
                ToolBlockState {
                    anthropic_index,
                    args_emitted: false,
                },
            );
        }

        if let Some(arguments) = tool_call.pointer("/function/arguments").and_then(Value::as_str) {
            if !arguments.is_empty() {
                if let Some(state) = self.tool_blocks.get_mut(&openai_index) {
                    state.args_emitted = true;
                    let payload = json!({
                        "type": "content_block_delta",
                        "index": state.anthropic_index,
                        "delta": {"type": "input_json_delta", "partial_json": arguments},
                    });
                    self.frame("content_block_delta", payload);
                }
            }
        }
    }

    fn handle_chunk(&mut self, chunk: &Value) {
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage_input_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .or(self.usage_input_tokens);
            self.usage_output_tokens = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .or(self.usage_output_tokens);
        }

        let choice = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| {
                choices
                    .iter()
                    .find(|c| c.get("index").and_then(Value::as_u64) == Some(0))
                    .or_else(|| choices.first())
            });

        let Some(choice) = choice else {
            return;
        };

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }

        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                self.handle_text_delta(content);
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    self.handle_tool_delta(tool_call);
                }
            }
        }
    }

    fn map_stop_reason(reason: Option<&str>) -> &'static str {
        match reason {
            Some("length") => "max_tokens",
            Some("tool_calls") | Some("function_call") => "tool_use",
            _ => "end_turn",
        }
    }

    fn finalize(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        self.ensure_message_start();
        self.close_text_block();

        let mut tool_indices: Vec<usize> = self
            .tool_blocks
            .values()
            .map(|state| state.anthropic_index)
            .collect();
        tool_indices.sort_unstable();
        for index in tool_indices {
            // Guarantee valid JSON input for tools that received no arguments.
            let payload = json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": ""},
            });
            self.frame("content_block_delta", payload);
            let payload = json!({"type": "content_block_stop", "index": index});
            self.frame("content_block_stop", payload);
        }

        let stop_reason = Self::map_stop_reason(self.finish_reason.as_deref());
        let output_tokens = self.usage_output_tokens.unwrap_or(0);
        let payload = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": output_tokens},
        });
        self.frame("message_delta", payload);

        self.frame("message_stop", json!({"type": "message_stop"}));
    }
}

/// Stream adapter: OpenAI `chat.completions` SSE bytes -> Anthropic SSE bytes.
#[pin_project]
pub struct OpenAiSseTransformer<S, E> {
    #[pin]
    inner: SseReframer<S, E>,
    state: TransformState,
}

impl<S, E> OpenAiSseTransformer<S, E> {
    pub fn new(inner: S, model: impl Into<String>, estimated_input_tokens: u32) -> Self {
        Self {
            inner: SseReframer::new(inner),
            state: TransformState::new(model.into(), estimated_input_tokens),
        }
    }
}

impl<S, E> Stream for OpenAiSseTransformer<S, E>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: Into<ProviderError>,
{
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            if let Some(bytes) = this.state.out.pop_front() {
                return Poll::Ready(Some(Ok(bytes)));
            }
            if this.state.done {
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => {
                    if event.data.trim() == "[DONE]" {
                        this.state.finalize();
                        continue;
                    }
                    match serde_json::from_str::<Value>(&event.data) {
                        Ok(chunk) => this.state.handle_chunk(&chunk),
                        Err(e) => {
                            tracing::debug!(
                                "Ignoring unparseable OpenAI SSE chunk: {} ({})",
                                event.data,
                                e
                            );
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    // Upstream ended (with or without [DONE]) — close out the
                    // Anthropic message so clients always see message_stop.
                    this.state.finalize();
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, StreamExt};

    fn run_transform(chunks: Vec<&str>) -> Vec<String> {
        let byte_chunks: Vec<Result<Bytes, ProviderError>> = chunks
            .into_iter()
            .map(|c| Ok(Bytes::from(c.to_string())))
            .collect();
        let stream = OpenAiSseTransformer::new(stream::iter(byte_chunks), "gpt-test", 7);
        let collected = futures::executor::block_on(stream.collect::<Vec<_>>());
        collected
            .into_iter()
            .map(|r| String::from_utf8_lossy(&r.unwrap()).to_string())
            .collect()
    }

    fn data_of(event: &str) -> &str {
        event
            .lines()
            .find(|l| l.starts_with("data: "))
            .and_then(|l| l.strip_prefix("data: "))
            .unwrap_or("")
    }

    fn types(events: &[String]) -> Vec<String> {
        events
            .iter()
            .map(|e| {
                e.lines()
                    .find(|l| l.starts_with("event: "))
                    .and_then(|l| l.strip_prefix("event: "))
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn test_text_stream_basic() {
        let chunks = vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"PO\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"NG\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        ];
        let events = run_transform(chunks);
        let kinds = types(&events);
        assert_eq!(kinds.first().unwrap(), "message_start");
        assert_eq!(kinds.last().unwrap(), "message_stop");
        assert!(kinds.contains(&"content_block_start".to_string()));
        assert_eq!(
            kinds.iter().filter(|k| *k == "content_block_delta").count(),
            2
        );
        let full: String = events
            .iter()
            .filter(|e| e.starts_with("event: content_block_delta"))
            .map(|e| {
                serde_json::from_str::<Value>(data_of(e))
                    .unwrap()["delta"]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(full, "PONG");
        let start = serde_json::from_str::<Value>(data_of(&events[0])).unwrap();
        assert_eq!(start["message"]["usage"]["input_tokens"], 7);
        let delta = events
            .iter()
            .find(|e| e.starts_with("event: message_delta"))
            .unwrap();
        let d = serde_json::from_str::<Value>(data_of(delta)).unwrap();
        assert_eq!(d["delta"]["stop_reason"], "end_turn");
        assert_eq!(d["usage"]["output_tokens"], 2);
    }

    #[test]
    fn test_tool_call_stream() {
        let chunks = vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me check.\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_time\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ];
        let events = run_transform(chunks);
        let kinds = types(&events);
        // Text block must be closed before the tool block opens.
        let tool_start_pos = events
            .iter()
            .position(|e| e.contains("\"type\":\"tool_use\""))
            .unwrap();
        assert_eq!(kinds[tool_start_pos - 1], "content_block_stop");
        let start = serde_json::from_str::<Value>(
            data_of(&events[tool_start_pos]),
        )
        .unwrap();
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(start["content_block"]["name"], "get_time");
        assert!(events.iter().any(|e| e.contains("input_json_delta")));
        let delta_evt = events
            .iter()
            .find(|e| e.contains("input_json_delta"))
            .unwrap();
        let d = serde_json::from_str::<Value>(data_of(delta_evt)).unwrap();
        assert_eq!(d["delta"]["partial_json"], "{}");
        let delta = events
            .iter()
            .find(|e| e.starts_with("event: message_delta"))
            .unwrap();
        let d = serde_json::from_str::<Value>(data_of(delta)).unwrap();
        assert_eq!(d["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn test_split_chunks_and_missing_done() {
        // Events split mid-frame, and no [DONE] sentinel at the end.
        let chunks = vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"cont",
            "ent\":\"HI\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"fin",
            "ish_reason\":\"stop\"}]}\n\n",
        ];
        let events = run_transform(chunks);
        let kinds = types(&events);
        assert_eq!(kinds.last().unwrap(), "message_stop");
        let text: String = events
            .iter()
            .filter(|e| e.starts_with("event: content_block_delta"))
            .map(|e| {
                serde_json::from_str::<Value>(data_of(e)).unwrap()["delta"]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(text, "HI");
    }

    #[test]
    fn test_empty_message_still_well_formed() {
        let events = run_transform(vec!["data: [DONE]\n\n"]);
        let kinds = types(&events);
        assert_eq!(kinds, vec!["message_start", "message_delta", "message_stop"]);
    }

    #[test]
    fn test_sse_frame_buffer_split_events() {
        let mut buf = SseFrameBuffer::new();
        assert!(buf.push("event: a\nda").is_empty());
        let events = buf.push("ta: {\"x\":1}\n\nevent: b\ndata: {\"y\":2}\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("a"));
        assert_eq!(events[1].data, "{\"y\":2}");
        let rest = buf.finish();
        assert!(rest.is_empty());
    }
}
