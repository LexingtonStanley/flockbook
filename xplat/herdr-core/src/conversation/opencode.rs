//! opencode 2.x, from the copy [`crate::opencode`] keeps of a session: an `{"opencode_session": …}` line, then one
//! `{"opencode_id", "opencode": message}` line per message, oldest first. A message is `user` (`text`, `files`),
//! `assistant` (`content`: `text`, `reasoning` and `tool` parts), `idle` (`outcome`: succeeded, failed or
//! interrupted), `compaction`, or a notice (`synthetic`, `system`, `model-switched`, `agent-switched`).
//!
//! Reasoning and notices are hidden. A tool part is a call with its `state` (`status`, `input`, then `content` or
//! `error`); it stays open, so it can show as pending, until its status is `completed` or `error`. A copy is written
//! again whole when a message changes, so a part's id is its message id and index.

use crate::model::{ConversationItem, ItemKind, ToolCall};
use crate::util::json::{self, Obj};
use super::{cut, label, pretty_json, summary, text, ConversationParser};

/// A line of an opencode copy: a message or the header.
pub fn is_opencode(o: &Obj) -> bool {
    json::obj(o.get("opencode")).is_some() || o.contains_key("opencode_session")
}

/// opencode's tools under Claude Code's names, so chips read the same. Unknown names stay as they are.
pub fn opencode_tool(name: &str) -> String {
    match name {
        "shell" | "bash" | "execute" => "Bash".into(),
        "read" => "Read".into(),
        "write" => "Write".into(),
        "edit" | "patch" | "multiedit" => "Edit".into(),
        "grep" => "Grep".into(),
        "glob" | "list" => "Glob".into(),
        "webfetch" => "WebFetch".into(),
        "websearch" => "WebSearch".into(),
        "subagent" | "task" => "Agent".into(),
        "todowrite" => "TodoWrite".into(),
        _ => name.into(),
    }
}

/// The line a chip shows for a call: opencode's own input names first (`path`, `code`), then the shared rule.
pub fn opencode_summary(name: &str, input: &Obj) -> String {
    let keys: &[&str] = match name {
        "Bash" => &["command", "code"],
        "Read" | "Write" | "Edit" => &["path", "filePath"],
        "Grep" | "Glob" => &["pattern"],
        "WebFetch" => &["url"],
        "WebSearch" => &["query"],
        "Agent" => &["description", "prompt"],
        _ => &[],
    };
    for k in keys {
        if let Some(s) = json::str_(input.get(*k)) {
            let first = text::first_line(s).unwrap_or("");
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    summary(name, input)
}

/// A tool's output: the text of its `content` parts, or for an error the `error`'s message.
fn tool_output(state: &Obj) -> (String, bool) {
    if json::str_(state.get("status")) == Some("error") {
        let e = state.get("error");
        let msg = json::str_(e)
            .or_else(|| json::str_(json::obj(e).and_then(|o| o.get("message"))))
            .unwrap_or("Error");
        return (msg.to_string(), true);
    }
    let out = json::objs(state.get("content"))
        .unwrap_or_default()
        .iter()
        .filter_map(|c| json::str_(c.get("text")))
        .collect::<Vec<_>>()
        .join("\n");
    (out, false)
}

/// Seconds since 1970 from opencode's milliseconds.
fn secs(v: Option<&serde_json::Value>) -> Option<f64> {
    json::f64_(v).map(|ms| ms / 1000.0).filter(|t| t.is_finite())
}

impl ConversationParser {
    pub(super) fn feed_opencode(&mut self, o: &Obj) {
        let Some(m) = json::obj(o.get("opencode")) else { return }; // the header
        let id = json::str_(m.get("id")).unwrap_or("");
        let created = secs(json::obj(m.get("time")).and_then(|t| t.get("created")));
        self.line_ts = created;
        let status = |p: &mut ConversationParser, key: &str, s: String| {
            p.flush();
            p.done.push(ConversationItem::new(&format!("oc-{id}-{key}"), ItemKind::Status(s), created));
        };
        match json::str_(m.get("type")) {
            Some("user") => {
                let t = text::trim(json::str_(m.get("text")).unwrap_or(""));
                if t.is_empty() {
                    return;
                }
                self.flush();
                self.done.push(ConversationItem::new(&format!("oc-{id}"), ItemKind::UserText(t.into()), created));
            }
            Some("assistant") => {
                for (i, p) in json::objs(m.get("content")).unwrap_or_default().into_iter().enumerate() {
                    let key = format!("oc-{id}-{i}");
                    match json::str_(p.get("type")) {
                        Some("text") => {
                            let t = text::trim(json::str_(p.get("text")).unwrap_or(""));
                            if t.is_empty() {
                                continue;
                            }
                            self.flush();
                            self.done.push(ConversationItem::new(&key, ItemKind::AssistantText(t.into()), created));
                        }
                        Some("tool") => self.open_opencode_call(p, &key, created),
                        _ => {} // reasoning: hidden, as for Claude Code
                    }
                }
                if json::str_(m.get("finish")) == Some("error") {
                    let msg = json::str_(json::obj(m.get("error")).and_then(|e| e.get("message"))).unwrap_or("");
                    let msg = text::first_line(msg).unwrap_or("");
                    let s = if msg.is_empty() { "Error".to_string() } else { format!("Error: {}", text::prefix(msg, 160)) };
                    status(self, "err", s);
                }
            }
            Some("idle") => match json::str_(m.get("outcome")) {
                Some("interrupted") => status(self, "end", "Interrupted".into()),
                // An error the turn ended with is on its last assistant message too: say it once.
                Some("failed") if !self.last_is_error() => status(self, "end", "Error".into()),
                _ => {}
            },
            Some("compaction") => status(self, "compact", "Context compacted".into()),
            _ => {} // notices: synthetic (a shell job's output), system, model-switched, agent-switched
        }
    }

    fn last_is_error(&self) -> bool {
        matches!(self.done.last().map(|i| &i.kind), Some(ItemKind::Status(s)) if s.starts_with("Error"))
    }

    /// One `tool` part: a call, with its output once it ended.
    fn open_opencode_call(&mut self, p: &Obj, key: &str, created: Option<f64>) {
        let empty = Obj::new();
        let state = json::obj(p.get("state")).unwrap_or(&empty);
        let input = json::obj(state.get("input")).unwrap_or(&empty);
        let name = opencode_tool(json::str_(p.get("name")).unwrap_or("tool"));
        let call_id = json::str_(p.get("id")).filter(|s| !s.is_empty()).unwrap_or(key).to_string();
        let sum = opencode_summary(&name, input);
        self.line_ts = secs(json::obj(p.get("time")).and_then(|t| t.get("created"))).or(created);
        if self.run.is_empty() {
            self.run_key = key.into();
        }
        let mut call = ToolCall::new(&call_id, &name, &sum, &cut(&pretty_json(input)));
        let ended = matches!(json::str_(state.get("status")), Some("completed") | Some("error"));
        if ended {
            let (out, is_error) = tool_output(state);
            call.output = Some(cut(&out));
            call.is_error = is_error;
        }
        self.open_call(&call_id, &name, label(&name), (!ended).then(|| sum.clone()), call);
        self.line_ts = created;
    }
}
