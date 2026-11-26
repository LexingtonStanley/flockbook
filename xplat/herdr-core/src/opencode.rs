//! opencode 2.x keeps its sessions behind a background server (`opencode service status`), not in a log file.
//! Flockbook reads them with the `opencode api` command, which signs in to that server, and keeps a copy of the
//! pane's session as JSON Lines: an `{"opencode_session": {id, exe, server}}` line, then one
//! `{"opencode_id", "opencode": message}` line per message, oldest first. The feed, its older pages and glance read the
//! copy like any other log (`conversation/opencode.rs` parses its lines); [`sync`] brings it up to date first.
//!
//! The pane's session ([`pick`]): `--session`/`-s` from the agent's command line; else the root session (no parent)
//! whose directory is the pane's cwd and that was used last, at or after the agent started. Two opencode panes in one
//! cwd without `--session`: none. `--standalone` runs a private server that `opencode api` cannot reach: none.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::resolver::Proc;
use crate::session_log::{home_dir, standardize_path};

/// Messages a sync asks for: the newest [`PAGE`]; for a new copy (or a gap) pages of [`FULL_PAGE`], the server's
/// most, up to [`MAX_PAGES`] of them.
pub const PAGE: usize = 50;
pub const FULL_PAGE: usize = 200;
pub const MAX_PAGES: usize = 10;
/// Sessions one lookup reads (the newest first).
pub const SESSIONS: usize = 100;
/// A session used this long before the agent started still counts (the two clocks, the first write).
const START_SLACK_MS: f64 = 5000.0;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// What the agent's command line says about its session and server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Launch {
    /// `--session <id>` / `-s <id>`.
    pub session: Option<String>,
    /// `--server <url>`: a server other than the background service.
    pub server: Option<String>,
    /// `--standalone`: a private server nothing else can reach.
    pub standalone: bool,
}

/// Reads [`Launch`] from the words of the agent's command line (an environment after them is skipped: no flag has
/// a `NAME=value` form).
pub fn launch(words: &[&str]) -> Launch {
    let mut l = Launch::default();
    let mut it = words.iter().skip(1);
    while let Some(w) = it.next() {
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v)),
            _ => (*w, None),
        };
        let mut value = || inline.map(str::to_string).or_else(|| it.next().map(|v| v.to_string())).filter(|v| !v.is_empty());
        match flag {
            "--session" | "-s" => l.session = value(),
            "--server" => l.server = value(),
            "--standalone" => l.standalone = true,
            _ => {}
        }
    }
    l
}

/// The words of a process's command line.
pub fn words(p: &Proc) -> Vec<&str> {
    if p.argv.is_empty() {
        p.line.split(' ').filter(|w| !w.is_empty()).collect()
    } else {
        p.argv.iter().map(String::as_str).collect()
    }
}

/// The `opencode` program to run `api` with: `FLOCKBOOK_OPENCODE`; on Linux the agent's own executable; its first
/// word when that is a path; else `opencode` from the PATH.
pub fn exe_for(agent: &Proc) -> String {
    if let Some(e) = std::env::var("FLOCKBOOK_OPENCODE").ok().filter(|e| !e.is_empty()) {
        return e;
    }
    #[cfg(target_os = "linux")]
    if let Ok(e) = std::fs::read_link(format!("/proc/{}/exe", agent.pid)) {
        // Not an interpreter's (`node …/opencode`): only opencode's own program answers `api`.
        if e.file_name().is_some_and(|n| n.to_string_lossy().starts_with("opencode")) {
            return e.to_string_lossy().into_owned();
        }
    }
    let first = words(agent).first().map(|w| w.to_string()).unwrap_or_default();
    if Path::new(&first).is_absolute() {
        return first;
    }
    // TODO(windows): an npm install puts `opencode.cmd` on the PATH, which Command does not run.
    "opencode".into()
}

/// One request to the server, as `opencode api` takes it (`session.list --param limit=5`): its JSON, or None when it
/// failed, timed out or did not print JSON.
pub trait Api {
    fn call(&self, args: &[String]) -> Option<Value>;
}

/// The server through `<exe> api … [--server <url>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cli {
    pub exe: String,
    pub server: Option<String>,
}

impl Api for Cli {
    fn call(&self, args: &[String]) -> Option<Value> {
        let mut c = Command::new(&self.exe);
        c.arg("api").args(args);
        if let Some(s) = &self.server {
            c.arg("--server").arg(s);
        }
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = c.spawn().ok()?;
        let mut out = child.stdout.take()?;
        // Read while it runs: a page of messages fills a pipe.
        let reader = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = out.read_to_end(&mut b);
            b
        });
        let began = Instant::now();
        let ok = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s.success(),
                Ok(None) if began.elapsed() < CALL_TIMEOUT => std::thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
            }
        };
        let b = reader.join().ok()?;
        if !ok {
            return None;
        }
        serde_json::from_slice(&b).ok()
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// The sessions to pick from, newest first: those in `cwd` (`session.list`'s `data`).
pub fn sessions(api: &dyn Api, cwd: &str) -> Option<Value> {
    api.call(&args(&["session.list", "--param", &format!("directory={cwd}"), "--param", &format!("limit={SESSIONS}")]))
}

/// The pane's session from `session.list` output (`{data: [session]}`): the root session in `cwd` used last, at or
/// after `started_ms`. `rivals`: another opencode pane runs in the same cwd, so the choice would be a guess: none.
pub fn pick(list: &Value, cwd: &str, started_ms: f64, rivals: bool) -> Option<String> {
    if rivals {
        return None;
    }
    let cwd = standardize_path(cwd);
    let mut best: Option<(f64, &str)> = None;
    for s in list.get("data").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = s.get("id").and_then(Value::as_str) else { continue };
        if s.get("parentID").is_some_and(|p| !p.is_null()) {
            continue;
        }
        let dir = s.get("location").and_then(|l| l.get("directory")).and_then(Value::as_str);
        if dir.map(standardize_path).as_deref() != Some(cwd.as_str()) {
            continue;
        }
        let t = s.get("time");
        let at = |k: &str| t.and_then(|t| t.get(k)).and_then(Value::as_f64).unwrap_or(0.0);
        let used = at("updated").max(at("viewed"));
        if used + START_SLACK_MS < started_ms || t.and_then(|t| t.get("archived")).is_some_and(|a| !a.is_null()) {
            continue;
        }
        if best.is_none_or(|(b, _)| used > b) {
            best = Some((used, id));
        }
    }
    best.map(|(_, id)| id.to_string())
}

/// Where the copies go when the helper names no folder.
pub fn default_dir() -> PathBuf {
    PathBuf::from(home_dir()).join(".cache").join("flockbook").join("opencode")
}

/// The copy of `session` in `dir`. Session ids are `ses_` and letters and digits; anything else is not a file name.
pub fn copy_path(dir: &Path, session: &str) -> Option<PathBuf> {
    session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-').then(|| dir.join(format!("{session}.jsonl")))
}

/// How a copy reaches its session: the copy's first line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub session: String,
    pub cli: Cli,
}

impl Header {
    pub fn line(&self) -> String {
        json!({"opencode_session": {"id": self.session, "exe": self.cli.exe, "server": self.cli.server}}).to_string()
    }

    pub fn parse(line: &str) -> Option<Header> {
        let v: Value = serde_json::from_str(line).ok()?;
        let h = v.get("opencode_session")?;
        Some(Header {
            session: h.get("id")?.as_str()?.to_string(),
            cli: Cli {
                exe: h.get("exe")?.as_str()?.to_string(),
                server: h.get("server").and_then(Value::as_str).map(str::to_string),
            },
        })
    }
}

/// A tool's output text kept in the copy, in bytes: the chat shows 4 KB of it (`util::cut`).
pub const KEEP_OUTPUT: usize = 16 * 1024;

/// A message as a line of the copy: its id first, so a sync finds it without parsing the line. What Flockbook never
/// shows stays out ([`slim`]).
pub fn message_line(m: &Value) -> Option<String> {
    let id = m.get("id")?.as_str()?.to_string();
    let mut m = m.clone();
    slim(&mut m);
    Some(format!("{{\"opencode_id\":{},\"opencode\":{}}}", json!(id), m))
}

/// Drops the provider's own fields (`native`: response ids, encrypted reasoning), attached files' data and the
/// workspace snapshot, and cuts tool outputs to [`KEEP_OUTPUT`].
pub fn slim(m: &mut Value) {
    let Some(o) = m.as_object_mut() else { return };
    o.remove("native");
    o.remove("snapshot");
    for f in o.get_mut("files").and_then(Value::as_array_mut).into_iter().flatten() {
        if let Some(f) = f.as_object_mut() {
            f.remove("data");
        }
    }
    for p in o.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
        let Some(p) = p.as_object_mut() else { continue };
        p.remove("native");
        let parts = p.get_mut("state").and_then(|s| s.get_mut("content")).and_then(Value::as_array_mut);
        for c in parts.into_iter().flatten() {
            if let Some(Value::String(t)) = c.get_mut("text") {
                if t.len() > KEEP_OUTPUT {
                    let mut at = KEEP_OUTPUT;
                    while !t.is_char_boundary(at) {
                        at -= 1;
                    }
                    t.truncate(at);
                    t.push('…');
                }
            }
        }
    }
}

/// The id of a copy's message line.
pub fn line_id(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("{\"opencode_id\":\"")?;
    rest.split('"').next()
}

/// One page of messages, oldest first, and the cursor to the page before it.
fn page(api: &dyn Api, session: &str, limit: usize, cursor: Option<&str>) -> Option<(Vec<String>, Option<String>)> {
    let mut a = args(&["session.message.list", "--param", &format!("sessionID={session}"), "--param", &format!("limit={limit}")]);
    if let Some(c) = cursor {
        a.extend(args(&["--param", &format!("cursor={c}")]));
    }
    let v = api.call(&a)?;
    let mut lines: Vec<String> = v.get("data")?.as_array()?.iter().filter_map(message_line).collect();
    lines.reverse(); // the server lists the newest first
    let older = v.get("cursor").and_then(|c| c.get("next")).and_then(Value::as_str).filter(|_| lines.len() >= limit).map(str::to_string);
    Some((lines, older))
}

/// The copy's messages brought up to date from the server: the newest page replaces the copy from its first message
/// on (a message that changed, or one opencode took back, goes with it). When the copy has none of the page's
/// messages, older pages are read until one overlaps, or the copy starts again from what was read. None: the server
/// did not answer.
pub fn merged(api: &dyn Api, session: &str, have: &[String]) -> Option<Vec<String>> {
    let limit = if have.is_empty() { FULL_PAGE } else { PAGE };
    let (mut got, mut older) = page(api, session, limit, None)?;
    for _ in 0..MAX_PAGES {
        if let Some(at) = got.first().and_then(|l| line_id(l)).and_then(|id| have.iter().position(|h| line_id(h) == Some(id))) {
            let mut out = have[..at].to_vec();
            out.append(&mut got);
            return Some(out);
        }
        let Some(c) = older.take() else { break };
        let (mut more, next) = page(api, session, FULL_PAGE, Some(&c))?;
        more.append(&mut got);
        got = more;
        older = next;
    }
    Some(got)
}

/// Brings the copy at `path` up to date, through the server its header names. Writes only when a message changed,
/// so an unchanged session keeps its size (`unchanged` for the feed). False: no header, or the server did not answer.
pub fn sync(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let mut lines = text.lines();
    let Some(h) = lines.next().and_then(Header::parse) else { return false };
    let have: Vec<String> = lines.map(str::to_string).collect();
    sync_with(path, &h, &have, &h.cli)
}

/// A copy synced this long ago is read as it is: the mod asks for the feed and glance every second or so, and each
/// sync is a call to the server.
pub const FRESH: Duration = Duration::from_millis(1500);

/// [`sync`] once per process, and not again within [`FRESH`]: the resolver syncs a copy when it finds it, and the
/// reads after it do not ask again.
pub fn sync_once(path: &Path) -> bool {
    if SYNCED.lock().map(|d| d.contains(path)).unwrap_or(false) || since_sync(path).is_some_and(|d| d < FRESH) {
        return true;
    }
    let ok = sync(path);
    synced(path, ok);
    ok
}

/// The file beside a copy whose time is its last sync (the copy's own time changes only when a message did: glance
/// keeps what it read by it).
fn stamp(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.synced"))
}

fn since_sync(path: &Path) -> Option<Duration> {
    std::fs::metadata(stamp(path)).and_then(|m| m.modified()).ok()?.elapsed().ok()
}

fn synced(path: &Path, ok: bool) {
    if let Ok(mut d) = SYNCED.lock() {
        d.insert(path.to_path_buf());
    }
    if ok {
        write_whole(&stamp(path), b"");
    }
}

/// The copies synced in this process.
static SYNCED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Makes or updates the copy at `path` for `h`'s session, reading through `api`. A new copy is written even when
/// the server does not answer (its header alone: "No messages yet").
pub fn start(path: &Path, h: &Header, api: &dyn Api) -> bool {
    if let Some(dir) = path.parent() {
        prune(dir, KEEP_UNUSED);
    }
    let old = std::fs::read_to_string(path).ok();
    let mut lines = old.as_deref().map(str::lines);
    let same = lines.as_mut().and_then(Iterator::next).and_then(Header::parse).as_ref() == Some(h);
    let have: Vec<String> = if same { lines.into_iter().flatten().map(str::to_string).collect() } else { vec![] };
    if !same {
        // a new copy, or one of another session or server: its header alone until the server answers
        write_whole(path, format!("{}\n", h.line()).as_bytes());
    }
    let ok = sync_with(path, h, &have, api);
    synced(path, ok);
    ok
}

/// A copy no pane read for this long is deleted (the session stays on opencode's server).
pub const KEEP_UNUSED: Duration = Duration::from_secs(14 * 24 * 3600);

/// Deletes the copies in `dir` not synced (or, without a stamp, not written) for longer than `age`.
pub fn prune(dir: &Path, age: Duration) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let ago = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("ses") && name.ends_with(".jsonl")) {
            continue;
        }
        if since_sync(&p).or_else(|| ago(&p)).is_some_and(|d| d > age) {
            let _ = std::fs::remove_file(stamp(&p));
            let _ = std::fs::remove_file(p);
        }
    }
}

fn sync_with(path: &Path, h: &Header, have: &[String], api: &dyn Api) -> bool {
    let Some(next) = merged(api, &h.session, have) else { return false };
    if next != have {
        let mut out = h.line();
        out.push('\n');
        for l in &next {
            out.push_str(l);
            out.push('\n');
        }
        write_whole(path, out.as_bytes());
    }
    true
}

/// Writes `d` whole through a temp file, for the owner only: a reader sees the old copy or the new one.
fn write_whole(path: &Path, d: &[u8]) {
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    let _ = b.create(dir);
    let tmp = dir.join(format!(".{}.{}", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let ok = o.open(&tmp).and_then(|mut f| std::io::Write::write_all(&mut f, d)).is_ok();
    if !ok || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The model the copy's newest assistant message used (`gpt-x`), for the log header.
pub fn model(lines: &[String]) -> Option<String> {
    for l in lines.iter().rev().filter(|l| l.starts_with("{\"opencode_id\":")) {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let m = v.get("opencode");
        if m.and_then(|m| m.get("type")).and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(id) = m.and_then(|m| m.get("model")).and_then(|m| m.get("id")).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Some(id.to_string());
        }
    }
    None
}
