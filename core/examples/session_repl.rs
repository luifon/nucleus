//! Prototype REPL for the new tmux-backed Session.
//!
//! Run:    cargo run -p nucleus-core --example session_repl
//! Watch:  tmux attach -t nucleus-test   (in another terminal)
//!
//! Each prompt at the `> ` is one message. Blank line = send. Multi-line
//! messages are supported — keep typing, then hit Enter on an empty line.
//! Ctrl-D / Ctrl-C to quit; the tmux window will be killed on clean exit.

use anyhow::Result;
use std::io::{self, BufRead, Write};
use std::time::Instant;

use nucleus_core::claude::PermissionMode;
use nucleus_core::claude_session::{AskOptions, Session, SpawnOptions};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();

    let workspace_root = std::env::current_dir()?;
    println!("spawning interactive claude in tmux session 'nucleus-test'…");
    let t0 = Instant::now();
    // Dev REPL — full literals on purpose (Default is gone per ADR-020;
    // real call sites go through session_profile).
    let mut session = Session::spawn(SpawnOptions {
        workspace_root: workspace_root.clone(),
        append_system_prompt: None,
        permission_mode: Some(PermissionMode::Auto),
        disallowed_tools: vec![],
        allowed_tools: vec![],
        add_dirs: vec![],
        tmux_session: "nucleus-test".into(),
        window_name: Some("repl".into()),
        ready_timeout: Duration::from_secs(20),
        resume_session_id: None,
        agent_label: None,
    })
    .await?;
    println!(
        "ready in {:.1}s — session_id = {}",
        t0.elapsed().as_secs_f64(),
        session.session_id()
    );
    println!("transcript = {}", session.transcript_path().display());
    println!("watch live: tmux attach -t nucleus-test");
    println!();
    println!("type a message; blank line to send. Ctrl-D to exit.");
    println!();

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut buf = String::new();

    loop {
        print!("> ");
        stdout.flush()?;
        let mut line = String::new();
        let read = stdin.lock().read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let line = line.trim_end_matches('\n').to_string();
        if line.is_empty() {
            if buf.is_empty() {
                continue;
            }
            let prompt_t0 = Instant::now();
            print!("(asking… ");
            stdout.flush()?;
            match session
                .ask(
                    &buf,
                    AskOptions {
                        max_wait: Duration::from_secs(180),
                        quiescent_window: Duration::from_secs(3),
                        await_turn_complete: false,
                    },
                )
                .await
            {
                Ok(reply) => {
                    let dt = prompt_t0.elapsed().as_secs_f64();
                    println!("{:.1}s)\n", dt);
                    println!("{}\n", reply);
                }
                Err(e) => {
                    println!("ERROR)\n  {}\n", e);
                }
            }
            buf.clear();
        } else {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&line);
        }
    }

    println!("\nclosing session…");
    session.close().await?;
    Ok(())
}
