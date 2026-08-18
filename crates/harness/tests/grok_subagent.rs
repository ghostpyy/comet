//! Grok subagent harvest, end to end against the fake ACP agent plus a
//! seeded grok session store: the spawn tool_call correlates with the
//! store via `rawOutput`'s subagent id, the transcript tails into tagged
//! [`AgentEvent::Subagent`] traffic (growing mid-run, like grok writes it),
//! and the store's completion markers close the chip with a tagged Done.
//! Own test binary: `ZERON_GROK_SESSIONS_DIR` is process-global.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel, ToolCall};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-acp.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

/// The parent session id the fake agent mints, and the subagent id its
/// spawn completion names (see `scenario:subagent` in fake-acp.sh).
const PARENT_SESSION: &str = "s-1";
const SUBAGENT_ID: &str = "sub-0001";

#[tokio::test]
async fn spawned_subagent_tails_the_store_into_tagged_events() {
    // Seed the store the way grok lays it out: a cwd dir (name is grok's
    // url-encoding — arbitrary here, the harness FINDS it by the parent
    // session dir inside), the child session dir with a growing
    // chat_history.jsonl, and the parent's subagents/<id>/ lifecycle dir.
    let store = tempfile::tempdir().expect("temp store");
    let cwd_dir = store.path().join("%2Ftmp%2Fdemo");
    let child_dir = cwd_dir.join(SUBAGENT_ID);
    let lifecycle = cwd_dir
        .join(PARENT_SESSION)
        .join("subagents")
        .join(SUBAGENT_ID);
    std::fs::create_dir_all(&child_dir).expect("child dir");
    std::fs::create_dir_all(&lifecycle).expect("lifecycle dir");
    let transcript = child_dir.join("chat_history.jsonl");
    std::fs::write(
        &transcript,
        concat!(
            r#"{"type":"system","content":"You are a subagent."}"#,
            "\n",
            r#"{"type":"user","content":"Read /w/PLAN.md and summarize."}"#,
            "\n",
            r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"Reading the plan."}],"status":"completed"}"#,
            "\n",
            r#"{"type":"assistant","content":"","tool_calls":[{"id":"call-r1","name":"read_file","arguments":"{\"path\":\"/w/PLAN.md\"}"}]}"#,
            "\n",
        ),
    )
    .expect("seed transcript");
    std::fs::write(
        lifecycle.join("meta.json"),
        format!(
            r#"{{"subagent_id":"{SUBAGENT_ID}","parent_session_id":"{PARENT_SESSION}","subagent_type":"explore","description":"Read the plan","status":"running"}}"#,
        ),
    )
    .expect("seed meta");
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("ZERON_GROK_SESSIONS_DIR", store.path());
    }

    // Mid-run growth: the subagent "finishes" while the harness tails —
    // final transcript lines land, then the terminal markers.
    let finish_transcript = transcript.clone();
    let finish_lifecycle = lifecycle.clone();
    let finisher = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&finish_transcript)
            .expect("append transcript");
        writeln!(
            f,
            r#"{{"type":"tool_result","tool_call_id":"call-r1","content":"1→plan body"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","content":"The plan is sound.","tool_calls":[]}}"#
        )
        .unwrap();
        drop(f);
        std::fs::write(
            finish_lifecycle.join("output.json"),
            r#"{"schema_version":1,"output":"The plan is sound."}"#,
        )
        .unwrap();
        std::fs::write(
            finish_lifecycle.join("meta.json"),
            format!(
                r#"{{"subagent_id":"{SUBAGENT_ID}","parent_session_id":"{PARENT_SESSION}","status":"completed"}}"#,
            ),
        )
        .unwrap();
    });

    let (_steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let request = RunRequest {
        prompt: "scenario:subagent".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    };
    let harness = AcpHarness::grok().with_executable(fixture_path());
    let stream = harness.run(request, controls).await.expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(20),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("run finished in time");
    finisher.await.expect("finisher ran");

    // The spawn chip registers under the cross-driver naming convention —
    // both the opening call and grok's bare-description retitle.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall { id, call: ToolCall::Unknown { name, .. } }
                if id == "call-sp-1" && name == "Agent: Read the plan"
        )),
        "spawn chip named after the task: {events:?}"
    );

    // Harvested interior, tagged to the spawn chip: reasoning, the typed
    // read_file call + its result, and the closing text.
    let tagged: Vec<&AgentEvent> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            } if parent_tool_use_id == "call-sp-1" => Some(event.as_ref()),
            _ => None,
        })
        .collect();
    assert!(
        tagged.iter().any(|e| matches!(
            e,
            AgentEvent::ReasoningDelta { text } if text.contains("Reading the plan.")
        )),
        "tagged reasoning: {events:?}"
    );
    assert!(
        tagged.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall { id, call: ToolCall::ReadFile { path } }
                if id == "call-r1" && path == "/w/PLAN.md"
        )),
        "tagged typed tool call: {events:?}"
    );
    assert!(
        tagged.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { id, output: Some(out), .. }
                if id == "call-r1" && out.contains("plan body")
        )),
        "tagged tool result: {events:?}"
    );
    assert!(
        tagged.iter().any(|e| matches!(
            e,
            AgentEvent::TextDelta { text } if text.contains("The plan is sound.")
        )),
        "tagged closing text: {events:?}"
    );

    // Exactly one tagged Done, completed, carrying the store's output —
    // and it is the LAST tagged event (transcript drained ahead of it).
    let dones: Vec<&AgentEvent> = tagged
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .copied()
        .collect();
    assert!(
        matches!(
            dones[..],
            [AgentEvent::Done {
                status: DoneStatus::Completed,
                result: Some(r),
                ..
            }] if r == "The plan is sound."
        ),
        "single completed tagged Done: {events:?}"
    );
    assert!(
        matches!(tagged.last(), Some(AgentEvent::Done { .. })),
        "Done closes the tagged stream: {events:?}"
    );

    // The interior stays OUT of the parent feed: no untagged event carries
    // the subagent's text (the pre-viz flat-fold bug).
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::TextDelta { text } if text.contains("The plan is sound.")
        )),
        "interior leaked untagged into the parent feed: {events:?}"
    );

    // The parent turn itself still settles cleanly.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )),
        "parent turn Done: {events:?}"
    );
}
