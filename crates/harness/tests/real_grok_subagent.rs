//! LIVE grok subagent harvest: drives the real `grok` CLI end to end and
//! asserts a background subagent's transcript arrives as tagged
//! [`AgentEvent::Subagent`] traffic closed by a tagged Done — the whole
//! session-store tail path against the vendor's actual disk format. Needs
//! an installed+authenticated grok; skipped otherwise. Run explicitly:
//!
//!   cargo test -p zeron-harness --test real_grok_subagent -- --ignored --nocapture

use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel, ToolCall, UserInputAnswer};

const PROMPT: &str = "Use spawn_subagent to launch ONE subagent of type explore with \
    description 'Probe listing' and prompt: 'Run the terminal command: ls /tmp and then \
    reply with the word finished.'. Then wait for its result with \
    get_command_or_subagent_output (positive timeout_ms) and summarize it in one line.";

#[tokio::test]
#[ignore = "drives the real grok binary; run with -- --ignored"]
async fn real_grok_background_subagent_harvests() {
    if std::process::Command::new("grok")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP: no grok binary on PATH");
        return;
    }

    let cwd = tempfile::tempdir().expect("cwd");
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Yes".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let request = RunRequest {
        prompt: PROMPT.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: cwd.path().display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    };

    let harness = AcpHarness::grok();
    let mut stream = harness.run(request, controls).await.expect("run starts");

    // Consume live until the tagged Done lands (the subagent may finish
    // before or after the parent turn settles), then end the run.
    let mut events: Vec<AgentEvent> = Vec::new();
    let mut tagged_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    while !tagged_done {
        let ev = tokio::select! {
            ev = stream.next() => match ev {
                Some(ev) => ev.expect("stream event"),
                None => break,
            },
            _ = tokio::time::sleep_until(deadline) => break,
        };
        eprintln!("{ev:?}");
        if let AgentEvent::Subagent { event, .. } = &ev {
            tagged_done = matches!(event.as_ref(), AgentEvent::Done { .. });
        }
        events.push(ev);
    }
    drop(steer_tx);
    token.cancel();
    while let Some(ev) = stream.next().await {
        if let Ok(ev) = ev {
            events.push(ev);
        }
    }

    // Registration: the spawn chip under the cross-driver name.
    let spawn_id = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCall {
                id,
                call: ToolCall::Unknown { name, .. },
            } if name.starts_with("Agent: ") => Some(id.clone()),
            _ => None,
        })
        .expect("spawn chip named 'Agent: …'");

    // Harvested interior, tagged to that chip, closed by a completed Done.
    let tagged: Vec<&AgentEvent> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            } if *parent_tool_use_id == spawn_id => Some(event.as_ref()),
            _ => None,
        })
        .collect();
    assert!(
        tagged
            .iter()
            .any(|e| !matches!(e, AgentEvent::Done { .. })),
        "tagged transcript content arrived: {events:?}"
    );
    assert!(
        tagged.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )),
        "tagged completed Done arrived: {events:?}"
    );
}
