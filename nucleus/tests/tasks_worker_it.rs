//! Background-task worker against a REAL tmux + claude session (ADR-033).
//! Opt-in (spends model turns, takes minutes):
//!
//!   cargo test -p nucleus --test tasks_worker_it -- --ignored --nocapture
//!
//! Runs `nucleus tasks start` in a temporary workspace whose nucleus.toml
//! points the workers at the `nucleus-test-tasks` tmux session, waits for the
//! detached worker to finish, and checks the ledger, the WhatsApp delivery
//! rows, and that the brief reached the model as typed input.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const TMUX: &str = "nucleus-test-tasks";

fn workspace() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nucleus-tasks-it-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    let dir = dir.canonicalize().unwrap();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nucleus.toml.example");
    let toml = std::fs::read_to_string(example)
        .unwrap()
        .replace("# tmux_session = \"nucleus-tasks\"", &format!("tmux_session = \"{TMUX}\""));
    assert!(toml.contains(TMUX), "nucleus.toml.example lost the [tasks] tmux_session line");
    std::fs::write(dir.join("nucleus.toml"), toml).unwrap();
    dir
}

fn nucleus(ws: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nucleus"))
        .args(args)
        .current_dir(ws)
        .env("NUCLEUS_USER_NAME", "Test Operator")
        .env("NUCLEUS_WORKSPACE_ROOT", ws)
        .env("NUCLEUS_TIER2_DIR", ws.join("memory"))
        .env("WHATSAPP_ALLOWED_DM_JIDS", "5511999999999")
        // This test runs as the operator, not as the Claude Code session
        // that may have started it.
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("NUCLEUS_TASK_SCOPE")
        .env_remove("NUCLEUS_TASK_WORKER")
        .output()
        .expect("running nucleus")
}

#[tokio::test]
#[ignore = "drives a real claude session; run with --ignored"]
async fn worker_runs_a_background_command_to_completion_and_delivers() {
    let ws = workspace();
    let brief = "Integration test of the background worker.\n\
        1. Start this Bash command IN THE BACKGROUND (run_in_background: true): \
        python3 -c \"import time; time.sleep(20); print('BG-DONE')\"\n\
        2. Wait for its completion notice.\n\
        3. Run this Bash command in the foreground: printenv NUCLEUS_TASK_WORKER\n\
        4. Your final message must be exactly: RESULT-OK <the text the first command printed> \
        WORKER-<the text the printenv command printed>";
    let out = nucleus(
        &ws,
        &["tasks", "start", "--title", "it worker", "--origin", "whatsapp-dm", "--requested-by", "cli", "--brief", brief],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "start failed: {stdout}\n{stderr}");
    println!("{stdout}");
    let short: String = stdout
        .lines()
        .find(|l| l.starts_with("started task "))
        .and_then(|l| l.split_whitespace().nth(2))
        .expect("task id in the start output")
        .to_string();

    let pool = nucleus_core::tasks::open(&ws).await.unwrap();
    let started = Instant::now();
    let task = loop {
        let t = nucleus_core::tasks::get(&pool, &short, &nucleus_core::tasks::Scope::Operator).await.unwrap();
        if t.status().is_terminal() {
            break t;
        }
        assert!(started.elapsed() < Duration::from_secs(600), "worker did not finish: {t:?}");
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    let events = nucleus_core::tasks::events(&pool, &task.id).await.unwrap();
    for e in &events {
        println!("{} {:<14} {}", e.at, e.kind, e.message.replace('\n', " "));
    }
    let _ = Command::new("tmux").args(["kill-session", "-t", TMUX]).output();

    assert_eq!(task.status, "done", "task: {task:?}");
    let result = task.result.clone().unwrap_or_default();
    assert!(result.contains("RESULT-OK") && result.contains("BG-DONE"), "result: {result}");
    assert!(result.contains(&format!("WORKER-{}", task.id)), "the worker session carries its role: {result}");
    assert!(events.iter().any(|e| e.kind == "background"), "the background command was tracked");
    // No bot runs here: the result is queued, and delivered only once the
    // bot marks the outbound row sent.
    assert!(events.iter().any(|e| e.kind == "delivery_queued"), "delivery queued");
    assert!(task.delivered_at.is_none(), "not delivered before the bot sends it");

    // Delivery rows for the bot.
    let wa = nucleus_core::whatsapp_queue::open(&ws).await.unwrap();
    let (body, target): (String, String) =
        sqlx::query_as("SELECT body, target FROM outbound_queue").fetch_one(&wa).await.unwrap();
    assert!(body.contains("done") && body.contains("BG-DONE"), "{body}");
    // Started from a shell without --origin-ref: both rows go to the bot's
    // operator-DM resolution.
    assert_eq!(target, "dm");
    let (payload, chat, sender): (String, String, String) =
        sqlx::query_as("SELECT payload, chat, sender FROM session_inbox").fetch_one(&wa).await.unwrap();
    assert_eq!(chat, "dm");
    assert!(sender.starts_with("task:"), "{sender}");
    assert!(payload.contains("BG-DONE") && !payload.contains("[agent-msg"), "the bot builds the envelope: {payload}");

    // The brief reached the model as a typed prompt, not a pasted block.
    let transcript = std::fs::read_to_string(task.transcript_path.unwrap()).unwrap();
    let prompt = transcript
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| {
            v["type"] == "user"
                && v["message"]["content"].as_str().map(|c| c.contains("Integration test of the background worker")).unwrap_or(false)
        })
        .expect("the brief is a plain user prompt");
    assert_eq!(prompt["promptSource"], "typed");
    assert!(!prompt["message"]["content"].as_str().unwrap().contains("<pasted_content"));
    let _ = std::fs::remove_dir_all(&ws);
}
