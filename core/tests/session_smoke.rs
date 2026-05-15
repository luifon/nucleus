//! Live smoke test for `claude_session::Session`.
//!
//! Run with:
//!   cargo test -p nucleus-core --test session_smoke -- --include-ignored --nocapture
//!
//! Marked `#[ignore]` so plain `cargo test` doesn't burn an interactive
//! claude session. Requires `tmux` + the `claude` binary on PATH.

use std::time::Instant;

use nucleus_core::claude::PermissionMode;
use nucleus_core::claude_session::{AskOptions, Session, SpawnOptions};

#[tokio::test]
#[ignore]
async fn spawn_ask_close_round_trip() {
    // Use the workspace root, not `core/` (which is cargo test's cwd).
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has a parent")
        .to_path_buf();

    let spawn_t0 = Instant::now();
    let mut session = Session::spawn(SpawnOptions {
        workspace_root,
        permission_mode: Some(PermissionMode::Auto),
        tmux_session: "nucleus-test-smoke".into(),
        window_name: Some("smoke".into()),
        ..SpawnOptions::default()
    })
    .await
    .expect("spawn");
    println!("[spawn] ready in {:.1}s — id {}", spawn_t0.elapsed().as_secs_f64(), session.session_id);

    let q1_t0 = Instant::now();
    let r1 = session
        .ask("Reply with exactly: PONG", AskOptions::default())
        .await
        .expect("ask 1");
    println!("[ask 1] {:.1}s → {:?}", q1_t0.elapsed().as_secs_f64(), r1);
    assert!(r1.to_uppercase().contains("PONG"), "expected PONG in reply, got: {}", r1);

    let q2_t0 = Instant::now();
    let r2 = session
        .ask(
            "What word did I just ask you to reply with? Output only that word.",
            AskOptions::default(),
        )
        .await
        .expect("ask 2");
    println!("[ask 2] {:.1}s → {:?}", q2_t0.elapsed().as_secs_f64(), r2);
    assert!(
        r2.to_uppercase().contains("PONG"),
        "session continuity broken — expected PONG, got: {}",
        r2
    );

    session.close().await.expect("close");
    println!("[close] ok");
}
