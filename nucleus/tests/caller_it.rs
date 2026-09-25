//! Caller detection of the operator CLIs from the real process tree
//! (ADR-033). Each case runs `nucleus` under a fake `claude` process started
//! with a Nucleus session's environment, with every Nucleus variable
//! removed from the command itself (`env -u …`): the start environment of
//! the `claude` process above the command decides, not the command's own.
//!
//! The fake is node with argv[0] "claude" (macOS hides the start environment
//! of platform binaries such as /bin/sh, and a real claude binary is not
//! one), detached from this test's ancestry so it is the outermost claude,
//! as in a tmux pane. Skipped when node is not installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const STRIP: &[&str] = &[
    "NUCLEUS_SESSION",
    "NUCLEUS_AGENT",
    "NUCLEUS_TASK_SCOPE",
    "NUCLEUS_TASK_WORKER",
    "CLAUDE_CODE_SESSION_ID",
];

fn node() -> Option<PathBuf> {
    let out = Command::new("/bin/sh").args(["-c", "command -v node"]).output().ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then(|| PathBuf::from(p))
}

fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nucleus-caller-it-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nucleus.toml.example");
    std::fs::copy(example, dir.join("nucleus.toml")).unwrap();
    std::fs::write(dir.join("agents.toml"), "[[agent]]\nname = \"whatsapp\"\nclass = \"conversational\"\nlaunch = \"launchd-daemon\"\nlaunchd_label = \"x\"\ntmux_session = \"nucleus-whatsapp\"\n").unwrap();
    dir.canonicalize().unwrap()
}

/// Run `nucleus <args>` under a fake claude started with `env`. Returns the
/// exit status and the combined output.
fn under_fake_claude(node: &Path, ws: &Path, env: &[(&str, &str)], args: &[&str]) -> (i32, String) {
    under_fake(node, ws, "claude", env, args)
}

/// Run `nucleus <args>` under a fake process named `argv0`.
fn under_fake(node: &Path, ws: &Path, argv0: &str, env: &[(&str, &str)], args: &[&str]) -> (i32, String) {
    let dir = tempfile_dir();
    let out = dir.join("out");
    let rc = dir.join("rc");
    let mut argv: Vec<String> = STRIP.iter().flat_map(|k| ["-u".to_string(), k.to_string()]).collect();
    argv.push(env!("CARGO_BIN_EXE_nucleus").to_string());
    argv.extend(args.iter().map(|a| a.to_string()));
    let fake = dir.join("fake.cjs");
    std::fs::write(
        &fake,
        format!(
            r#"const {{ spawnSync }} = require("node:child_process");
const fs = require("node:fs");
const until = Date.now() + 10000;
while (process.ppid !== 1 && Date.now() < until) spawnSync("sleep", ["0.05"]);
const r = spawnSync("env", {argv}, {{ encoding: "utf8", cwd: {ws}, stdio: ["ignore", "pipe", "pipe"] }});
fs.writeFileSync({out}, (r.stdout || "") + (r.stderr || ""));
fs.writeFileSync({rc}, String(r.status));
"#,
            argv = serde_json::to_string(&argv).unwrap(),
            ws = serde_json::to_string(&ws).unwrap(),
            out = serde_json::to_string(&out).unwrap(),
            rc = serde_json::to_string(&rc).unwrap(),
        ),
    )
    .unwrap();
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", &format!("(exec -a {argv0} '{}' '{}') &", node.display(), fake.display())])
        .env("NUCLEUS_WORKSPACE_ROOT", ws)
        .env("NUCLEUS_USER_NAME", "Test Operator")
        .env("NUCLEUS_TIER2_DIR", ws.join("memory"))
        .env("WHATSAPP_ALLOWED_DM_JIDS", "5511999999999")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.status().unwrap();
    let start = Instant::now();
    while !rc.exists() {
        assert!(start.elapsed() < Duration::from_secs(60), "the fake session did not finish");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(50));
    let status = std::fs::read_to_string(&rc).unwrap().trim().parse().unwrap_or(-1);
    let text = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    (status, text)
}

fn tempfile_dir() -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("nucleus-fake-claude-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn scope_hash(token: &str) -> String {
    nucleus_core::caller::scope_token_hash(token)
}

#[tokio::test]
async fn nucleus_sessions_are_recognized_from_the_process_tree() {
    let Some(node) = node() else {
        eprintln!("skipped: node is not installed");
        return;
    };
    let ws = workspace("tree");
    // whatsapp.db with one valid scope, as the bot records it.
    let wa = nucleus_core::whatsapp_queue::open(&ws).await.unwrap();
    sqlx::query("CREATE TABLE IF NOT EXISTS task_scopes (token_sha256 TEXT PRIMARY KEY, chat_id TEXT NOT NULL, created_at TEXT NOT NULL)")
        .execute(&wa)
        .await
        .unwrap();
    sqlx::query("INSERT INTO task_scopes VALUES (?1, '5511999999999@s.whatsapp.net', 'x')")
        .bind(scope_hash("good-token"))
        .execute(&wa)
        .await
        .unwrap();
    wa.close().await;
    let list = ["tasks", "list"];

    // A worker session stays a worker after `env -u NUCLEUS_TASK_WORKER`.
    let (rc, out) = under_fake_claude(&node, &ws, &[("NUCLEUS_SESSION", "worker"), ("NUCLEUS_TASK_WORKER", "ab12cd34")], &list);
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("workers cannot use the tasks CLI"), "{out}");

    // A chat session's scope comes from its start environment.
    let chat = |tok: &'static str| vec![("NUCLEUS_SESSION", "chat"), ("NUCLEUS_AGENT", "whatsapp"), ("NUCLEUS_TASK_SCOPE", tok)];
    let (rc, out) = under_fake_claude(&node, &ws, &chat("revoked-token"), &list);
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("no valid task scope"), "{out}");
    let (rc, out) = under_fake_claude(&node, &ws, &chat("good-token"), &list);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("no tasks"), "{out}");

    // Any other Nucleus session is not the operator.
    let agent = [("NUCLEUS_SESSION", "agent"), ("NUCLEUS_AGENT", "distiller")];
    let (rc, out) = under_fake_claude(&node, &ws, &agent, &list);
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("cannot use background tasks"), "{out}");
    // …and cannot claim another sender with --from.
    let (rc, out) = under_fake_claude(
        &node,
        &ws,
        &agent,
        &["session-send", "--to", "whatsapp-dm", "--from", "main", "--message", "hello"],
    );
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("does not match this session's agent"), "{out}");

    // A marked process that is not claude (a tmux server a worker started
    // keeps the worker's environment) still decides.
    let (rc, out) = under_fake(
        &node,
        &ws,
        "tmux-server",
        &[("NUCLEUS_SESSION", "worker"), ("NUCLEUS_TASK_WORKER", "ab12cd34")],
        &list,
    );
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("workers cannot use the tasks CLI"), "{out}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn tasks_run_needs_the_launch_token() {
    let ws = workspace("run");
    let pool = nucleus_core::tasks::open(&ws).await.unwrap();
    let t = nucleus_core::tasks::create(
        &pool,
        nucleus_core::tasks::NewTask {
            kind: "general".into(),
            title: "t".into(),
            brief: "b".into(),
            origin: "cli".into(),
            origin_ref: None,
            parent_id: None,
            requested_by: "cli".into(),
            links: vec![],
            workdir: None,
            profile: nucleus_core::tasks::WorkerProfile::Agentic,
        },
        &nucleus_core::tasks::Scope::Operator,
    )
    .await
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_nucleus"))
        .args(["tasks", "run", &t.id, "--workspace-root"])
        .arg(&ws)
        .current_dir(&ws)
        .env("NUCLEUS_WORKSPACE_ROOT", &ws)
        .env("NUCLEUS_USER_NAME", "Test Operator")
        .env("NUCLEUS_TIER2_DIR", ws.join("memory"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("`tasks run` is internal"), "{text}");
    let t = nucleus_core::tasks::get(&pool, &t.id, &nucleus_core::tasks::Scope::Operator).await.unwrap();
    assert_eq!(t.status, "queued", "the task was not taken");
    let _ = std::fs::remove_dir_all(&ws);
}
