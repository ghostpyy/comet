//! Grok subagent lifecycle: correlates `spawn_subagent` tool calls with the
//! transcript grok writes to its session store, and feeds the per-subagent
//! doc pipeline ([`AgentEvent::Subagent`]).
//!
//! Grok streams subagent interiors over ACP UNTAGGED — `parent_tool_use_id`
//! is null on every frame (verified live, 1.0.0 and 1.0.4; the docs promise
//! tagging but it is unimplemented) — so the wire alone cannot attribute
//! nested traffic to its spawn chip, and for BACKGROUND subagents the
//! interior never reaches the parent stream at all. The durable surface is
//! the session store: grok appends each subagent's full transcript to
//! `~/.grok/sessions/<urlenc-cwd>/<subagent_id>/chat_history.jsonl`
//! incrementally (flush-verified mid-run), and the parent session dir gains
//! `subagents/<subagent_id>/{meta.json,output.json}` — meta at spawn,
//! output only at completion.
//!
//! One tail task per spawned subagent maps those typed JSONL lines onto
//! tagged events: a message-level transcript (grok's disk granularity —
//! token-level deltas don't exist on this surface), then exactly one tagged
//! `Done` when the store marks the subagent finished. Correlation key: the
//! spawn tool_call carries `_meta["x.ai/tool"].name == "spawn_subagent"`,
//! and its completion update's `rawOutput.text` names the `subagent_id`.
//! The cwd dir is FOUND (the dir whose name grok minted from the session
//! cwd contains the parent session id) rather than re-deriving grok's
//! URL-encoding — one convention to drift instead of two.
//!
//! The store format is vendor-private: every read here fails SOFT (drift
//! degrades to chip-only, never errors the chat), and the tasks are aborted
//! at run end so a wedged subagent can't hold the event channel open past
//! its session.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

use zeron_proto::{AgentEvent, DoneStatus, HarnessId, ToolCall};

use super::normalize::{OUTPUT_CAP, cap_text};
use crate::HarnessError;

/// Poll cadence for the transcript tail. The disk is written at message
/// granularity, so sub-second polling buys nothing.
const POLL: Duration = Duration::from_millis(500);

/// Bound on spawn-completion → the subagent's dirs existing on disk (live
/// runs show ~1s). Past this the store layout has drifted: give up quietly.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(60);

/// The vendor marker riding every shaped update of the spawn tool call
/// (`_meta["x.ai/tool"].name` + `subagentBackground` on the opening call).
pub(crate) fn is_spawn(update: &Value) -> bool {
    update
        .get("_meta")
        .and_then(|m| m.get("x.ai/tool"))
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        == Some("spawn_subagent")
}

/// The spawned subagent's id, from the completion update's `rawOutput`
/// (`{type: "Text", text: "Subagent started in background.\nsubagent_id:
/// <uuid>\n…"}`). A first-class `rawOutput.subagent_id` is accepted too in
/// case grok ever promotes it. The id becomes a doc-id suffix downstream,
/// so anything shaped unlike an id is rejected rather than passed along.
pub(crate) fn subagent_id_from_update(update: &Value) -> Option<String> {
    let raw = update.get("rawOutput")?;
    let id = raw
        .get("subagent_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            raw.get("text")
                .and_then(Value::as_str)?
                .lines()
                .find_map(|l| {
                    l.strip_prefix("subagent_id:")
                        .map(|rest| rest.trim().to_owned())
                })
        })?;
    let plausible = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    plausible.then_some(id)
}

/// The per-notification entry: guards harness/session/shape, then scans.
/// Must be called from EVERY path that folds live `session/update`s —
/// besides the main notification arm, the turn-settle and steer-injection
/// drains each replay updates the prompt response outraced (grok's
/// `prompt_complete` settlement makes that the COMMON path for updates in
/// a turn's tail, the spawn completion included). `session/load` replay is
/// dropped during setup and never reaches any of them, so a resumed
/// session never re-harvests subagents its doc already holds.
pub(crate) fn scan_notification(
    harness: HarnessId,
    method: &str,
    params: &Value,
    session_id: &str,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    pending_spawns: &mut HashSet<String>,
    harvesters: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    if harness == HarnessId::Grok
        && method == "session/update"
        && params.get("sessionId").and_then(Value::as_str) == Some(session_id)
        && let Some(update) = params.get("update")
    {
        scan_update(update, session_id, event_tx, pending_spawns, harvesters);
    }
}

/// Track spawn tool calls across a session's `session/update`s and launch a
/// harvester when one resolves with a subagent id.
fn scan_update(
    update: &Value,
    parent_session_id: &str,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    pending_spawns: &mut HashSet<String>,
    harvesters: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
        return;
    };
    if is_spawn(update) {
        pending_spawns.insert(id.to_owned());
    }
    if !pending_spawns.contains(id) {
        return;
    }
    match update.get("status").and_then(Value::as_str) {
        Some("completed") => {
            pending_spawns.remove(id);
            // A completion without a parseable id (foreground shape, format
            // drift) harvests nothing — the chip resolves normally.
            if let Some(subagent_id) = subagent_id_from_update(update) {
                harvesters.push(tokio::spawn(harvest(
                    event_tx.clone(),
                    id.to_owned(),
                    parent_session_id.to_owned(),
                    subagent_id,
                )));
            }
        }
        Some("failed") => {
            pending_spawns.remove(id);
        }
        _ => {}
    }
}

/// The grok session store root. Overridable for tests and rigs.
fn sessions_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ZERON_GROK_SESSIONS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".grok").join("sessions"))
}

/// The per-cwd dir holding this run's sessions: the one that contains the
/// parent session's dir. Scanning sidesteps grok's cwd URL-encoding.
async fn find_cwd_dir(root: &Path, parent_session_id: &str) -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir(root).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let dir = entry.path();
        if tokio::fs::metadata(dir.join(parent_session_id))
            .await
            .is_ok_and(|m| m.is_dir())
        {
            return Some(dir);
        }
    }
    None
}

fn tag(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.to_owned(),
        event: Box::new(event),
    }
}

/// Tail one subagent to completion: locate its dirs, stream transcript
/// lines as tagged events, close with a tagged `Done` when the parent
/// session's `subagents/<id>/` entry reports a terminal status.
async fn harvest(
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    parent_tool_use_id: String,
    parent_session_id: String,
    subagent_id: String,
) {
    let Some(root) = sessions_root() else { return };
    let deadline = tokio::time::Instant::now() + DISCOVER_TIMEOUT;
    let cwd_dir = loop {
        if let Some(dir) = find_cwd_dir(&root, &parent_session_id).await {
            break dir;
        }
        if event_tx.is_closed() || tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(POLL).await;
    };
    let transcript = cwd_dir.join(&subagent_id).join("chat_history.jsonl");
    let lifecycle = cwd_dir
        .join(&parent_session_id)
        .join("subagents")
        .join(&subagent_id);

    let mut offset: u64 = 0;
    let mut carry: Vec<u8> = Vec::new();
    loop {
        for event in drain_lines(&transcript, &mut offset, &mut carry).await {
            if !send(&event_tx, tag(&parent_tool_use_id, event)).await {
                return;
            }
        }
        if let Some(done) = terminal_state(&lifecycle).await {
            // The final transcript lines land before the output marker;
            // one more drain keeps them ahead of the Done.
            for event in drain_lines(&transcript, &mut offset, &mut carry).await {
                if !send(&event_tx, tag(&parent_tool_use_id, event)).await {
                    return;
                }
            }
            let _ = send(&event_tx, tag(&parent_tool_use_id, done)).await;
            return;
        }
        if event_tx.is_closed() {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// Read complete new lines past `offset`, mapping each to events. A partial
/// trailing line (grok mid-append) carries over to the next poll.
async fn drain_lines(transcript: &Path, offset: &mut u64, carry: &mut Vec<u8>) -> Vec<AgentEvent> {
    let Ok(mut file) = tokio::fs::File::open(transcript).await else {
        return Vec::new();
    };
    if file.seek(std::io::SeekFrom::Start(*offset)).await.is_err() {
        return Vec::new();
    }
    let mut fresh = Vec::new();
    let Ok(read) = file.read_to_end(&mut fresh).await else {
        return Vec::new();
    };
    *offset += read as u64;
    carry.extend_from_slice(&fresh);

    let mut events = Vec::new();
    while let Some(nl) = carry.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = carry.drain(..=nl).collect();
        if let Ok(value) = serde_json::from_slice::<Value>(&line[..nl]) {
            events.extend(line_events(&value));
        }
    }
    events
}

/// Terminal signal: `output.json` exists (written only at completion), or
/// `meta.json` left the running state (covers cancellation shapes that may
/// never produce output). Status → [`DoneStatus`] mapping is permissive —
/// an unrecognized terminal word still closes the chip, as failed.
async fn terminal_state(lifecycle: &Path) -> Option<AgentEvent> {
    let meta_status = read_json(&lifecycle.join("meta.json"))
        .await
        .and_then(|m| m.get("status").and_then(Value::as_str).map(str::to_owned));
    let output = read_json(&lifecycle.join("output.json"))
        .await
        .and_then(|o| o.get("output").and_then(Value::as_str).map(str::to_owned));

    let status_word = match (&meta_status, &output) {
        (Some(s), _) if !matches!(s.as_str(), "running" | "pending" | "started") => s.clone(),
        (_, Some(_)) => meta_status.unwrap_or_else(|| "completed".into()),
        _ => return None,
    };
    let status = match status_word.as_str() {
        "completed" | "complete" | "succeeded" | "success" => DoneStatus::Completed,
        "canceled" | "cancelled" | "interrupted" | "stopped" | "killed" => DoneStatus::Interrupted,
        _ => DoneStatus::Errored,
    };
    Some(AgentEvent::Done {
        status,
        result: output.map(|o| cap_text(&o, OUTPUT_CAP)),
        error: None,
        session_id: None,
    })
}

async fn read_json(path: &Path) -> Option<Value> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Map one `chat_history.jsonl` line to transcript events. Message-level:
/// an `assistant` line is its text (if any) plus its tool calls; a
/// `reasoning` line is its summary; a `tool_result` resolves its call.
/// `system`/`user` lines are context the spawn chip already names.
fn line_events(line: &Value) -> Vec<AgentEvent> {
    match line.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            let mut events = Vec::new();
            if let Some(text) = text_content(line.get("content")) {
                events.push(AgentEvent::TextDelta {
                    text: format!("{text}\n\n"),
                });
            }
            for tc in line
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
            {
                let Some(id) = tc.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let name = tc.get("name").and_then(Value::as_str).unwrap_or("tool");
                events.push(AgentEvent::ToolCall {
                    id: id.to_owned(),
                    call: typed_grok_tool(name, tool_arguments(tc)),
                });
            }
            events
        }
        Some("reasoning") => {
            let joined: Vec<&str> = line
                .get("summary")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(|s| s.get("text").and_then(Value::as_str))
                .filter(|t| !t.is_empty())
                .collect();
            if joined.is_empty() {
                Vec::new()
            } else {
                vec![AgentEvent::ReasoningDelta {
                    text: format!("{}\n\n", joined.join("\n\n")),
                }]
            }
        }
        Some("tool_result") => {
            let Some(id) = line.get("tool_call_id").and_then(Value::as_str) else {
                return Vec::new();
            };
            vec![AgentEvent::ToolResult {
                id: id.to_owned(),
                is_error: false,
                output: text_content(line.get("content")).map(|t| cap_text(&t, OUTPUT_CAP)),
                diff: None,
            }]
        }
        _ => Vec::new(),
    }
}

/// Grok stores tool arguments as a JSON-encoded STRING; accept an inline
/// object too in case the encoding ever changes.
fn tool_arguments(tc: &Value) -> Option<Value> {
    match tc.get("arguments")? {
        Value::String(s) => serde_json::from_str(s).ok(),
        v @ Value::Object(_) => Some(v.clone()),
        _ => None,
    }
}

/// Content is a plain string or an array mixing strings and `{text}` blocks.
fn text_content(content: Option<&Value>) -> Option<String> {
    let joined = match content? {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(s) => Some(s.as_str()),
                obj => obj.get("text").and_then(Value::as_str),
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!joined.is_empty()).then_some(joined)
}

/// The handful of grok-internal tool names worth a typed chip; the rest
/// render by name.
fn typed_grok_tool(name: &str, input: Option<Value>) -> ToolCall {
    let arg = |key: &str| -> Option<String> {
        input
            .as_ref()?
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match name {
        "run_terminal_command" => ToolCall::Exec {
            command: arg("command").unwrap_or_default(),
        },
        "read_file" => ToolCall::ReadFile {
            path: arg("path")
                .or_else(|| arg("file_path"))
                .or_else(|| arg("target_file"))
                .unwrap_or_default(),
        },
        _ => ToolCall::Unknown {
            name: name.to_owned(),
            input,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spawn_call() -> Value {
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-abc-1",
            "title": "spawn_subagent",
            "rawInput": { "description": "Probe sleeper", "subagent_type": "explore" },
            "_meta": {
                "x.ai/tool": { "name": "spawn_subagent", "kind": "task" },
                "subagentBackground": true,
            },
        })
    }

    #[test]
    fn spawn_marker_reads_vendor_meta() {
        assert!(is_spawn(&spawn_call()));
        assert!(!is_spawn(&json!({
            "_meta": { "x.ai/tool": { "name": "get_command_or_subagent_output" } },
        })));
        assert!(!is_spawn(&json!({ "toolCallId": "t1" })));
    }

    #[test]
    fn subagent_id_parses_from_raw_output_text() {
        // Verbatim completion shape from a live grok 1.0.4 session.
        let update = json!({
            "toolCallId": "call-abc-1",
            "status": "completed",
            "rawOutput": {
                "type": "Text",
                "text": "Subagent started in background.\nsubagent_id: 01a01457-4f92-78c3-8a4f-914b975717b6\ntype: explore\ndescription: Probe sleeper",
            },
        });
        assert_eq!(
            subagent_id_from_update(&update).as_deref(),
            Some("01a01457-4f92-78c3-8a4f-914b975717b6")
        );
        // Promoted first-class field wins if it ever appears.
        assert_eq!(
            subagent_id_from_update(&json!({ "rawOutput": { "subagent_id": "abc-123" } }))
                .as_deref(),
            Some("abc-123")
        );
        // Id-shaped only: a path or sentence must not become a doc id.
        assert_eq!(
            subagent_id_from_update(&json!({
                "rawOutput": { "text": "subagent_id: /tmp/evil path" },
            })),
            None
        );
        assert_eq!(subagent_id_from_update(&json!({ "rawOutput": {} })), None);
    }

    #[test]
    fn assistant_line_maps_text_and_tool_calls() {
        let line = json!({
            "type": "assistant",
            "content": "Reading the file now.",
            "tool_calls": [{
                "id": "call-t1",
                "name": "read_file",
                "arguments": "{\"path\":\"/tmp/a.txt\"}",
            }],
        });
        let events = line_events(&line);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            AgentEvent::TextDelta { text } if text == "Reading the file now.\n\n"
        ));
        assert!(matches!(
            &events[1],
            AgentEvent::ToolCall { id, call: ToolCall::ReadFile { path } }
                if id == "call-t1" && path == "/tmp/a.txt"
        ));
    }

    #[test]
    fn assistant_array_content_and_exec_typing() {
        let line = json!({
            "type": "assistant",
            "content": ["Done.", ""],
            "tool_calls": [{
                "id": "call-t2",
                "name": "run_terminal_command",
                "arguments": "{\"command\":\"ls -la\"}",
            }],
        });
        let events = line_events(&line);
        assert!(matches!(
            &events[0],
            AgentEvent::TextDelta { text } if text == "Done.\n\n"
        ));
        assert!(matches!(
            &events[1],
            AgentEvent::ToolCall { call: ToolCall::Exec { command }, .. } if command == "ls -la"
        ));
    }

    #[test]
    fn reasoning_summary_joins_to_one_delta() {
        let line = json!({
            "type": "reasoning",
            "summary": [
                { "type": "summary_text", "text": "First thought." },
                { "type": "summary_text", "text": "Second thought." },
            ],
        });
        let events = line_events(&line);
        assert!(matches!(
            &events[..],
            [AgentEvent::ReasoningDelta { text }]
                if text == "First thought.\n\nSecond thought.\n\n"
        ));
    }

    #[test]
    fn tool_result_resolves_call() {
        let line = json!({
            "type": "tool_result",
            "tool_call_id": "call-t1",
            "content": "1→hello",
        });
        let events = line_events(&line);
        assert!(matches!(
            &events[..],
            [AgentEvent::ToolResult { id, is_error: false, output: Some(out), .. }]
                if id == "call-t1" && out == "1→hello"
        ));
    }

    #[test]
    fn system_user_and_junk_lines_are_silent() {
        assert!(line_events(&json!({ "type": "system", "content": "prompt" })).is_empty());
        assert!(line_events(&json!({ "type": "user", "content": "hi" })).is_empty());
        assert!(line_events(&json!({ "unexpected": true })).is_empty());
        assert!(line_events(&json!("not an object")).is_empty());
    }

    #[tokio::test]
    async fn scan_update_launches_harvester_on_spawn_completion() {
        let (tx, _rx) = mpsc::channel(8);
        let mut pending = HashSet::new();
        let mut harvesters = Vec::new();

        scan_update(&spawn_call(), "sess-1", &tx, &mut pending, &mut harvesters);
        assert!(pending.contains("call-abc-1"));
        assert!(harvesters.is_empty());

        // Unrelated tool completing must not harvest.
        scan_update(
            &json!({ "toolCallId": "call-other", "status": "completed",
                     "rawOutput": { "text": "subagent_id: not-for-you" } }),
            "sess-1",
            &tx,
            &mut pending,
            &mut harvesters,
        );
        assert!(harvesters.is_empty());

        scan_update(
            &json!({ "toolCallId": "call-abc-1", "status": "completed",
                     "rawOutput": { "text": "Subagent started in background.\nsubagent_id: 01a-1\n" } }),
            "sess-1",
            &tx,
            &mut pending,
            &mut harvesters,
        );
        assert_eq!(harvesters.len(), 1);
        assert!(!pending.contains("call-abc-1"));
        for h in harvesters {
            h.abort();
        }
    }

    #[tokio::test]
    async fn failed_spawn_never_harvests() {
        let (tx, _rx) = mpsc::channel(8);
        let mut pending = HashSet::new();
        let mut harvesters = Vec::new();
        scan_update(&spawn_call(), "sess-1", &tx, &mut pending, &mut harvesters);
        scan_update(
            &json!({ "toolCallId": "call-abc-1", "status": "failed" }),
            "sess-1",
            &tx,
            &mut pending,
            &mut harvesters,
        );
        assert!(harvesters.is_empty());
        assert!(pending.is_empty());
    }
}
