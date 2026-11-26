//! opencode: the copy's lines as a conversation, the pane's session, and keeping the copy in step with the server.
//! All fixtures are synthetic, in the shapes opencode 2.4's `session.list` and `session.message.list` return.

use std::cell::RefCell;

use herdr_core::conversation::ConversationParser;
use herdr_core::opencode::{self, Api, Header};
use herdr_core::{ConversationItem, ItemKind, PermState};
use serde_json::{json, Value};

fn parse(messages: &[Value]) -> Vec<ConversationItem> {
    let mut p = ConversationParser::new();
    p.feed(&json!({"opencode_session": {"id": "ses_a", "exe": "opencode", "server": null}}).to_string());
    for m in messages {
        p.feed(&opencode::message_line(m).expect("a message has an id"));
    }
    p.items()
}

fn describe(items: &[ConversationItem]) -> Vec<String> {
    items
        .iter()
        .map(|i| match &i.kind {
            ItemKind::UserText(t) => format!("user:{t}"),
            ItemKind::AssistantText(t) => format!("asst:{t}"),
            ItemKind::ToolChips(c) => format!("chips:{}", c.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join("|")),
            ItemKind::PermissionRequest(p) => format!("perm:{}:{}", p.tool_name, p.command),
            ItemKind::Question(_) => "question".into(),
            ItemKind::Status(s) => format!("status:{s}"),
        })
        .collect()
}

fn user(id: &str, at: u64, text: &str) -> Value {
    json!({"id": id, "time": {"created": at}, "type": "user", "text": text, "files": [], "agents": []})
}

fn assistant(id: &str, at: u64, content: Value, finish: &str) -> Value {
    json!({"id": id, "time": {"created": at, "completed": at + 5}, "type": "assistant", "agent": "build",
        "model": {"id": "model-x", "providerID": "p", "variant": "high"}, "content": content, "finish": finish,
        "native": {"responseId": "resp_1"}, "cost": 0, "tokens": {"input": 1, "output": 1}})
}

fn tool(id: &str, name: &str, input: Value, status: &str, out: &str) -> Value {
    let mut state = json!({"status": status, "input": input, "metadata": {}});
    match status {
        "completed" => state["content"] = json!([{"type": "text", "text": out}]),
        "error" => state["error"] = json!({"type": "unknown", "message": out}),
        _ => {}
    }
    json!({"type": "tool", "id": id, "name": name, "executed": false, "state": state, "time": {"created": 1_000_100}})
}

#[test]
fn a_turn_reads_as_user_text_chips_and_the_answer() {
    let items = parse(&[
        user("msg_1", 1_000_000, "  Fix the build  "),
        assistant(
            "msg_2",
            1_000_010,
            json!([
                {"type": "reasoning", "text": "**Thinking**", "native": {"reasoningEncryptedContent": "x"}},
                tool("call_1", "shell", json!({"command": "cargo build", "workdir": "/w"}), "completed", "ok"),
                tool("call_2", "read", json!({"path": "/w/src/main.rs"}), "completed", "fn main() {}"),
            ]),
            "tool-calls",
        ),
        assistant("msg_3", 1_000_020, json!([{"type": "text", "text": "Done."}]), "stop"),
        json!({"id": "msg_4", "time": {"created": 1_000_030}, "type": "idle", "outcome": "succeeded"}),
    ]);
    assert_eq!(describe(&items), ["user:Fix the build", "chips:Ran a command|Read a file", "asst:Done."]);
    assert_eq!(items[0].timestamp, Some(1000.0), "opencode's milliseconds as seconds");
    let ItemKind::ToolChips(chips) = &items[1].kind else { panic!("chips") };
    let bash = &chips[0].calls[0];
    assert_eq!((bash.tool_name.as_str(), bash.summary.as_str(), bash.output.as_deref()), ("Bash", "cargo build", Some("ok")));
    assert_eq!(chips[1].calls[0].summary, "/w/src/main.rs", "opencode's `path` names the file");
}

#[test]
fn a_running_tool_is_pending_and_a_failed_one_is_an_error() {
    let items = parse(&[
        user("msg_1", 1_000_000, "go"),
        assistant(
            "msg_2",
            1_000_010,
            json!([
                tool("call_1", "edit", json!({"path": "/w/a.rs", "oldString": "a", "newString": "b"}), "error", "no match"),
                tool("call_2", "shell", json!({"command": "make test"}), "running", ""),
            ]),
            "tool-calls",
        ),
    ]);
    let d = describe(&items);
    assert_eq!(d, ["user:go", "chips:Edited a file", "perm:Bash:make test"], "the open call is the pending item");
    let ItemKind::ToolChips(chips) = &items[1].kind else { panic!("chips") };
    assert!(chips[0].calls[0].is_error && chips[0].calls[0].output.as_deref() == Some("no match"));
    let ItemKind::PermissionRequest(p) = &items[2].kind else { panic!("perm") };
    assert_eq!(p.state, PermState::Pending);
}

#[test]
fn errors_interruptions_and_compaction_are_status_lines() {
    let err = json!({"type": "api", "message": "rate limited\nretry later", "status": 429});
    let mut failed = assistant("msg_2", 1_000_010, json!([]), "error");
    failed["error"] = err;
    let items = parse(&[
        user("msg_1", 1_000_000, "one"),
        failed,
        json!({"id": "msg_3", "time": {"created": 1_000_020}, "type": "idle", "outcome": "failed", "error": {}}),
        user("msg_4", 1_000_030, "two"),
        json!({"id": "msg_5", "time": {"created": 1_000_040}, "type": "idle", "outcome": "interrupted"}),
        json!({"id": "msg_6", "time": {"created": 1_000_050}, "type": "compaction", "status": "completed", "reason": "auto", "summary": "s"}),
    ]);
    assert_eq!(
        describe(&items),
        ["user:one", "status:Error: rate limited", "user:two", "status:Interrupted", "status:Context compacted"],
        "a failed turn says its error once"
    );
}

#[test]
fn notices_and_empty_messages_are_hidden() {
    let items = parse(&[
        json!({"id": "msg_1", "time": {"created": 1}, "type": "system", "text": "instructions", "description": "d", "metadata": {}}),
        json!({"id": "msg_2", "time": {"created": 2}, "type": "synthetic", "text": "job output", "description": "d", "metadata": {"source": "shell"}}),
        json!({"id": "msg_3", "time": {"created": 3}, "type": "model-switched", "model": {"id": "a"}, "previous": {"id": "b"}}),
        json!({"id": "msg_4", "time": {"created": 4}, "type": "agent-switched", "agent": "plan", "previous": "build"}),
        user("msg_5", 5, "   "),
        assistant("msg_6", 6, json!([{"type": "text", "text": ""}, {"type": "reasoning", "text": "hidden"}]), "stop"),
    ]);
    assert!(items.is_empty(), "{:?}", describe(&items));
}

#[test]
fn ids_stay_the_same_when_the_copy_is_written_again() {
    let msgs = [user("msg_1", 1, "a"), assistant("msg_2", 2, json!([{"type": "text", "text": "b"}]), "stop")];
    let ids = |items: Vec<ConversationItem>| items.into_iter().map(|i| i.id).collect::<Vec<_>>();
    let once = ids(parse(&msgs));
    let with_more = ids(parse(&[msgs[0].clone(), msgs[1].clone(), user("msg_3", 3, "c")]));
    assert_eq!(once, with_more[..2], "a message's items keep their ids");
}

#[test]
fn the_command_line_names_the_session_and_server() {
    let l = opencode::launch(&["opencode", "-s", "ses_x", "--server=http://h:1", "/w", "HOME=/home/u"]);
    assert_eq!((l.session.as_deref(), l.server.as_deref(), l.standalone), (Some("ses_x"), Some("http://h:1"), false));
    let l = opencode::launch(&["opencode", "--session=ses_y", "--standalone"]);
    assert_eq!((l.session.as_deref(), l.standalone), (Some("ses_y"), true));
    assert_eq!(opencode::launch(&["opencode", "-c"]), opencode::Launch::default(), "--continue: the server's choice, found by use");
}

fn session(id: &str, dir: &str, updated: f64, parent: Option<&str>) -> Value {
    let mut s = json!({"id": id, "location": {"directory": dir}, "time": {"created": updated - 10.0, "updated": updated}});
    if let Some(p) = parent {
        s["parentID"] = json!(p);
    }
    s
}

#[test]
fn the_pane_gets_the_root_session_in_its_folder_used_last_since_it_started() {
    let list = json!({"data": [
        session("ses_old", "/w", 1_000.0, None),
        session("ses_new", "/w", 9_000.0, None),
        session("ses_sub", "/w", 9_500.0, Some("ses_new")),
        session("ses_else", "/elsewhere", 9_900.0, None),
        session("ses_mid", "/w/", 8_000.0, None),
    ]});
    assert_eq!(opencode::pick(&list, "/w", 7_000.0, false).as_deref(), Some("ses_new"), "a subagent's session is not the pane's");
    assert_eq!(opencode::pick(&list, "/w", 20_000.0, false), None, "nothing used since the agent started: no conversation yet");
    assert_eq!(opencode::pick(&list, "/w", 9_004.0, false).as_deref(), Some("ses_new"), "a few seconds' slack");
    assert_eq!(opencode::pick(&list, "/w", 7_000.0, true), None, "two opencode in one folder: no guess");
    let mut viewed = list.clone();
    viewed["data"][4]["time"]["viewed"] = json!(9_800.0);
    assert_eq!(opencode::pick(&viewed, "/w", 7_000.0, false).as_deref(), Some("ses_mid"), "viewed counts as used");
}

#[test]
fn copy_paths_are_only_for_session_ids() {
    let dir = std::path::Path::new("/c");
    assert_eq!(opencode::copy_path(dir, "ses_Ab1").map(|p| p.to_string_lossy().into_owned()), Some("/c/ses_Ab1.jsonl".into()));
    assert_eq!(opencode::copy_path(dir, "../x"), None);
}

#[test]
fn the_copy_keeps_only_what_the_chat_shows() {
    let big = "é".repeat(opencode::KEEP_OUTPUT);
    let mut m = assistant("msg_1", 1, json!([tool("c", "shell", json!({"command": "x"}), "completed", &big)]), "stop");
    m["content"][0]["native"] = json!({"itemId": "i"});
    m["snapshot"] = json!("abc");
    let line = opencode::message_line(&m).expect("line");
    assert_eq!(opencode::line_id(&line), Some("msg_1"));
    let v: Value = serde_json::from_str(&line).expect("json");
    let o = &v["opencode"];
    assert!(o.get("native").is_none() && o.get("snapshot").is_none() && o["content"][0].get("native").is_none());
    let out = o["content"][0]["state"]["content"][0]["text"].as_str().expect("text");
    assert!(out.len() <= opencode::KEEP_OUTPUT + '…'.len_utf8() && out.ends_with('…'));
    let mut u = user("msg_2", 2, "see");
    u["files"] = json!([{"name": "a.png", "mime": "image/png", "data": "AAAA", "source": {}}]);
    let v: Value = serde_json::from_str(&opencode::message_line(&u).expect("line")).expect("json");
    assert!(v["opencode"]["files"][0].get("data").is_none() && v["opencode"]["files"][0]["name"] == "a.png");
}

/// A server with `messages` (oldest first) that pages as opencode does: the newest first, `cursor.next` to older.
struct Server {
    messages: RefCell<Vec<Value>>,
    calls: RefCell<Vec<String>>,
}

impl Server {
    fn new(n: usize) -> Server {
        Server { messages: RefCell::new((0..n).map(|i| user(&format!("msg_{i:04}"), i as u64, &format!("m{i}"))).collect()), calls: RefCell::new(vec![]) }
    }
}

impl Api for Server {
    fn call(&self, args: &[String]) -> Option<Value> {
        self.calls.borrow_mut().push(args.join(" "));
        let param = |k: &str| args.iter().find_map(|a| a.strip_prefix(&format!("{k}=")).map(str::to_string));
        let limit: usize = param("limit")?.parse().ok()?;
        let end: usize = param("cursor").map(|c| c.parse().expect("cursor")).unwrap_or(self.messages.borrow().len());
        let start = end.saturating_sub(limit);
        let data: Vec<Value> = self.messages.borrow()[start..end].iter().rev().cloned().collect();
        Some(json!({"data": data, "cursor": {"next": if start > 0 { Some(start.to_string()) } else { None }}}))
    }
}

fn ids(lines: &[String]) -> Vec<String> {
    lines.iter().filter_map(|l| opencode::line_id(l)).map(str::to_string).collect()
}

#[test]
fn a_new_copy_reads_older_pages_and_a_sync_only_the_newest() {
    let s = Server::new(450);
    let all = opencode::merged(&s, "ses_a", &[]).expect("copy");
    assert_eq!(all.len(), 450);
    assert_eq!(ids(&all).first().map(String::as_str), Some("msg_0000"), "oldest first");
    assert_eq!(s.calls.borrow().len(), 3, "three pages of 200");

    s.messages.borrow_mut().push(user("msg_0450", 450, "new"));
    s.messages.borrow_mut()[449] = user("msg_0449", 449, "changed");
    s.calls.borrow_mut().clear();
    let next = opencode::merged(&s, "ses_a", &all).expect("sync");
    assert_eq!(s.calls.borrow().len(), 1, "one page");
    assert_eq!(next.len(), 451);
    assert!(next[449].contains("changed") && next[..400] == all[..400]);

    s.messages.borrow_mut().truncate(440);
    let back = opencode::merged(&s, "ses_a", &next).expect("sync");
    assert_eq!(ids(&back).last().map(String::as_str), Some("msg_0439"), "messages opencode took back go");
}

#[test]
fn a_gap_longer_than_a_page_reads_back_until_it_meets_the_copy() {
    let s = Server::new(100);
    let have = opencode::merged(&s, "ses_a", &[]).expect("copy");
    for i in 100..230 {
        s.messages.borrow_mut().push(user(&format!("msg_{i:04}"), i, "x"));
    }
    s.calls.borrow_mut().clear();
    let next = opencode::merged(&s, "ses_a", &have).expect("sync");
    assert_eq!(next.len(), 230);
    assert_eq!(ids(&next), (0..230).map(|i| format!("msg_{i:04}")).collect::<Vec<_>>());
    assert_eq!(s.calls.borrow().len(), 2, "the newest 50, then one older page");
}

#[test]
fn start_writes_the_header_and_keeps_the_copy_of_the_same_session() {
    let dir = tempfile::tempdir().expect("dir");
    let path = opencode::copy_path(dir.path(), "ses_a").expect("path");
    let h = Header { session: "ses_a".into(), cli: opencode::Cli { exe: "/x/opencode".into(), server: None } };
    let s = Server::new(3);
    assert!(opencode::start(&path, &h, &s));
    let text = std::fs::read_to_string(&path).expect("copy");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(Header::parse(lines[0]).as_ref(), Some(&h));
    assert_eq!(lines.len(), 4);
    let before = std::fs::metadata(&path).and_then(|m| m.modified()).expect("mtime");
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(opencode::start(&path, &h, &s));
    assert_eq!(std::fs::metadata(&path).and_then(|m| m.modified()).expect("mtime"), before, "nothing changed: not written");
    let other = Header { session: "ses_a".into(), cli: opencode::Cli { exe: "/x/opencode".into(), server: Some("http://h".into()) } };
    struct Down;
    impl Api for Down {
        fn call(&self, _: &[String]) -> Option<Value> {
            None
        }
    }
    assert!(!opencode::start(&path, &other, &Down), "the server did not answer");
    assert_eq!(std::fs::read_to_string(&path).expect("copy").lines().count(), 1, "another server's copy starts again from its header");
}

#[test]
fn the_model_is_the_newest_assistant_messages() {
    let lines = vec![
        Header { session: "ses_a".into(), cli: opencode::Cli { exe: "o".into(), server: None } }.line(),
        opencode::message_line(&assistant("msg_1", 1, json!([]), "stop")).expect("line"),
        opencode::message_line(&user("msg_2", 2, "x")).expect("line"),
    ];
    assert_eq!(opencode::model(&lines).as_deref(), Some("model-x"));
}
