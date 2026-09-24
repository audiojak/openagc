//! `claude -p --output-format stream-json` → [`AgentEvent`]s (spec §9.3).
//!
//! Text arrives twice: as partial deltas (`stream_event`) and in the final
//! `assistant` message. Deltas drive the UI; the full message is used only
//! for tool calls, whose arguments are complete there.

use std::collections::HashSet;

use agent_api::{AgentEvent, Usage, shorten, summarize_args};
use serde_json::Value;

const TOOL_PREFIX: &str = "mcp__openagc__";

#[derive(Debug, Default)]
pub struct StreamParser {
    session_id: Option<String>,
    started_tools: HashSet<String>,
    finished: bool,
    mcp_checked: bool,
}

fn tool_name(name: &str) -> String {
    name.strip_prefix(TOOL_PREFIX).unwrap_or(name).to_owned()
}

impl StreamParser {
    /// The CLI's session id, once, when first seen.
    pub fn take_session_id(&mut self) -> Option<String> {
        self.session_id.take()
    }

    /// Whether the turn's `result` arrived.
    pub fn finished(&self) -> bool {
        self.finished
    }

    pub fn parse_line(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { return vec![] };
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => self.init(&v),
            Some("stream_event") => self.stream_event(&v["event"]),
            Some("assistant") => self.assistant(&v["message"]),
            Some("user") => self.tool_results(&v["message"]),
            Some("result") => self.result(&v),
            _ => vec![],
        }
    }

    fn init(&mut self, v: &Value) -> Vec<AgentEvent> {
        if let Some(id) = v["session_id"].as_str() {
            self.session_id = Some(id.to_owned());
        }
        if self.mcp_checked {
            return vec![];
        }
        self.mcp_checked = true;
        // Without the openagc server the agent can do nothing useful.
        let servers = v["mcp_servers"].as_array().cloned().unwrap_or_default();
        let ours = servers.iter().find(|s| s["name"] == "openagc");
        match ours.and_then(|s| s["status"].as_str()) {
            Some("connected") | None if ours.is_some() => vec![],
            Some(status) => {
                vec![AgentEvent::TextDelta { text: format!("⚠︎ OpenAGC's mail tools did not start ({status}). ") }]
            }
            None => vec![AgentEvent::TextDelta { text: "⚠︎ OpenAGC's mail tools are not available. ".into() }],
        }
    }

    fn stream_event(&mut self, e: &Value) -> Vec<AgentEvent> {
        match e["type"].as_str() {
            Some("content_block_delta") => match e["delta"]["type"].as_str() {
                Some("text_delta") => {
                    let text = e["delta"]["text"].as_str().unwrap_or_default();
                    if text.is_empty() { vec![] } else { vec![AgentEvent::TextDelta { text: text.to_owned() }] }
                }
                Some("thinking_delta") => {
                    let text = e["delta"]["thinking"].as_str().unwrap_or_default();
                    if text.is_empty() { vec![] } else { vec![AgentEvent::ThinkingDelta { text: text.to_owned() }] }
                }
                _ => vec![],
            },
            _ => vec![],
        }
    }

    fn assistant(&mut self, message: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for block in message["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_use" {
                let id = block["id"].as_str().unwrap_or_default().to_owned();
                if self.started_tools.insert(id.clone()) {
                    out.push(AgentEvent::ToolCallStarted {
                        call_id: id,
                        tool: tool_name(block["name"].as_str().unwrap_or_default()),
                        args_summary: summarize_args(&block["input"]),
                    });
                }
            }
        }
        out
    }

    fn tool_results(&mut self, message: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for block in message["content"].as_array().into_iter().flatten() {
            if block["type"] != "tool_result" {
                continue;
            }
            let text = match &block["content"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
                _ => String::new(),
            };
            out.push(AgentEvent::ToolCallFinished {
                call_id: block["tool_use_id"].as_str().unwrap_or_default().to_owned(),
                ok: !block["is_error"].as_bool().unwrap_or(false),
                summary: shorten(&text, 160),
            });
        }
        out
    }

    fn result(&mut self, v: &Value) -> Vec<AgentEvent> {
        self.finished = true;
        if let Some(id) = v["session_id"].as_str() {
            self.session_id = Some(id.to_owned());
        }
        let failed = v["is_error"].as_bool().unwrap_or(false) || v["subtype"].as_str().is_some_and(|s| s != "success");
        if failed {
            let message = match v["subtype"].as_str() {
                Some("error_max_turns") => "The agent reached its turn limit.".to_owned(),
                Some("error_max_budget_usd") => "The agent reached its spending limit.".to_owned(),
                _ => v["result"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("The agent stopped with an error.")
                    .to_owned(),
            };
            return vec![AgentEvent::TurnFailed { message }];
        }
        let usage = &v["usage"];
        let usage = usage.is_object().then(|| Usage {
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
            cached_input_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
        });
        vec![AgentEvent::TurnCompleted { usage, cost_usd: v["total_cost_usd"].as_f64() }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = r#"{"type":"system","subtype":"init","session_id":"sess-1","tools":["mcp__openagc__mail_search"],"mcp_servers":[{"name":"openagc","status":"connected"}]}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me search."}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Looking"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" now."}}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Looking now."},{"type":"tool_use","id":"toolu_1","name":"mcp__openagc__mail_search","input":{"query":"is:unread","limit":5}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"{\"threads\":[]}"}]}]}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Nothing unread."}}}
{"type":"result","subtype":"success","is_error":false,"result":"Nothing unread.","session_id":"sess-1","total_cost_usd":0.0123,"usage":{"input_tokens":1200,"output_tokens":40,"cache_read_input_tokens":800}}"#;

    #[test]
    fn a_turn_maps_to_events() {
        let mut p = StreamParser::default();
        let events: Vec<AgentEvent> = TRANSCRIPT.lines().flat_map(|l| p.parse_line(l)).collect();
        assert_eq!(p.take_session_id().as_deref(), Some("sess-1"));
        assert!(p.finished());
        assert_eq!(
            events,
            vec![
                AgentEvent::ThinkingDelta { text: "Let me search.".into() },
                AgentEvent::TextDelta { text: "Looking".into() },
                AgentEvent::TextDelta { text: " now.".into() },
                AgentEvent::ToolCallStarted {
                    call_id: "toolu_1".into(),
                    tool: "mail_search".into(),
                    args_summary: "limit: 5, query: is:unread".into(),
                },
                AgentEvent::ToolCallFinished {
                    call_id: "toolu_1".into(),
                    ok: true,
                    summary: "{\"threads\":[]}".into()
                },
                AgentEvent::TextDelta { text: "Nothing unread.".into() },
                AgentEvent::TurnCompleted {
                    usage: Some(Usage { input_tokens: 1200, output_tokens: 40, cached_input_tokens: 800 }),
                    cost_usd: Some(0.0123),
                },
            ]
        );
    }

    #[test]
    fn failures_and_a_missing_mcp_server() {
        let mut p = StreamParser::default();
        let e = p.parse_line(r#"{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"s"}"#);
        assert_eq!(e, vec![AgentEvent::TurnFailed { message: "The agent reached its turn limit.".into() }]);

        let mut p = StreamParser::default();
        let e = p.parse_line(r#"{"type":"system","subtype":"init","session_id":"s","mcp_servers":[{"name":"openagc","status":"failed"}]}"#);
        assert!(matches!(&e[..], [AgentEvent::TextDelta { text }] if text.contains("failed")));
        assert!(p.parse_line("not json").is_empty());
    }

    #[test]
    fn argument_summaries_are_short_and_single_line() {
        let long = serde_json::json!({ "body_markdown": "line one\nline two ".repeat(40), "thread_ids": ["a", "b"] });
        let s = summarize_args(&long);
        assert!(s.chars().count() <= 121 && !s.contains('\n'), "{s}");
        assert_eq!(summarize_args(&serde_json::json!({ "thread_ids": ["a"] })), "thread_ids: 1 item");
    }
}
