//! Which Claude Code session, if any, a process runs inside (ADR-033).
//!
//! The operator CLIs (`nucleus tasks`, `nucleus session-send`, the WhatsApp
//! send scripts) decide what a caller may do from where the caller runs. The
//! caller's own environment is not evidence: a session's tool command can
//! remove or change any variable before it runs a CLI. This module reads
//! facts a tool command cannot change:
//!
//! - **The process tree.** A tool command is a descendant of the `claude`
//!   process that runs the session. Every session Nucleus starts runs its
//!   `claude` with `NUCLEUS_SESSION=<kind>` (and `NUCLEUS_AGENT`,
//!   `NUCLEUS_TASK_SCOPE`, `NUCLEUS_TASK_WORKER` as applicable) in the
//!   environment it was started with. The environment a process was started
//!   with is fixed at `exec`; its descendants cannot edit it. The outermost
//!   ancestor that carries the marker decides, `claude` or not: a session
//!   can start processes below itself with any environment, but it cannot
//!   insert one above itself. A process the session started that kept its
//!   environment (a tmux server, for example) carries the marker too, so
//!   the commands in that server's panes are still the session's.
//! - **The controlling terminal.** A command that detaches from its parent
//!   (`(cmd &)`, `nohup`) is re-parented to launchd and loses the ancestor
//!   chain, but keeps the terminal of the tmux pane it ran in. When no
//!   ancestor is a `claude` process or carries the marker, a Nucleus
//!   `claude` process on the same terminal identifies the session.
//!
//! `claude` processes are recognized by fixed names (`claude`, Claude Code's
//! `…/claude/versions/…` install path), never by a variable of the caller's
//! environment.
//!
//! What remains is a process that detached from its terminal too (a new
//! session via `setsid`): it is [`Origin::Detached`], which the CLIs refuse
//! for everything except harmless maintenance. A process that cannot be
//! classified because the process table could not be read is
//! [`Origin::Unknown`], and the CLIs refuse it.
//!
//! These checks run as the operator's macOS user, like every Nucleus session.
//! They do not isolate a session. A session that starts a process with a
//! new terminal and with the Nucleus variables removed from its environment
//! (for example a new detached tmux server started with `env -u …`, or a
//! new window of a tmux server that runs outside the session) runs that
//! process with no marked ancestor and no Nucleus session on its terminal,
//! so it is classified as the operator's terminal. So does a session that
//! writes into another process's memory or edits the CLIs. What such a
//! process can still not do is send WhatsApp messages to any chat other
//! than the operator's DM and the configured groups: every sending path
//! checks the target (`messaging/whatsapp/src/target_policy.ts`). ADR-033
//! records these limits; closing them needs an OS sandbox around the
//! sessions.
//!
//! The classification ([`classify`]) is pure and shared with
//! `messaging/whatsapp/src/proc_tree.ts`: both run the vectors in
//! `core/testdata/caller_origin_vectors.json`.

use serde::Deserialize;
use std::collections::BTreeMap;

/// Kind of Nucleus session, set by every Nucleus spawn (`chat`, `worker`,
/// `braindump`, `job`, `agent`).
pub const ENV_SESSION: &str = "NUCLEUS_SESSION";
/// Session kind of a background task worker.
pub const SESSION_WORKER: &str = "worker";
/// Session kind of a WhatsApp chat session.
pub const SESSION_CHAT: &str = "chat";
/// Session kind every other Rust-spawned session gets by default.
pub const SESSION_AGENT: &str = "agent";

/// The variables that identify a Nucleus session. A process Nucleus starts
/// outside a session (the detached task worker, a tmux server) must not
/// inherit them from the session that started it.
pub const SESSION_VARS: &[&str] = &[
    ENV_SESSION,
    crate::agent_msg::ENV_AGENT,
    crate::caller::ENV_TASK_SCOPE,
    crate::caller::ENV_TASK_WORKER,
    crate::caller::ENV_CLAUDE_SESSION,
];

/// One process of the tree, as the classifier needs it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Node {
    pub pid: i32,
    /// The process is a `claude` executable.
    pub claude: bool,
    /// The `NUCLEUS_*` variables of the environment the process started
    /// with. `None`: the environment could not be read.
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    /// Session id from the process's own arguments (`--session-id <id>` or
    /// `--resume <id>`), for `claude` processes.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// What [`classify`] reads.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Snapshot {
    /// Controlling terminal of the calling process; `None` when it has none.
    pub tty: Option<String>,
    /// Ancestors, nearest first (the parent first), up to but not including
    /// the root process.
    pub ancestors: Vec<Node>,
    /// `claude` processes whose controlling terminal is `tty`.
    #[serde(default)]
    pub tty_peers: Vec<Node>,
    /// False when the ancestor chain could not be read to the root.
    #[serde(default = "yes")]
    pub complete: bool,
}

fn yes() -> bool {
    true
}

/// A Nucleus-started session, from the environment its `claude` started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NucleusSession {
    pub pid: i32,
    pub kind: String,
    pub agent: Option<String>,
    pub scope: Option<String>,
    pub worker: Option<String>,
    pub session_id: Option<String>,
}

impl NucleusSession {
    pub fn is_worker(&self) -> bool {
        self.kind == SESSION_WORKER || self.worker.is_some()
    }
}

/// Where the calling process runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A terminal of the operator: no `claude` ancestor, no ancestor with
    /// the Nucleus marker, and no Nucleus session on the same terminal.
    Terminal,
    /// A Claude Code session the operator started (a `claude` ancestor, and
    /// no ancestor with the Nucleus marker).
    OperatorSession { session_id: Option<String> },
    /// A session Nucleus started.
    Nucleus(NucleusSession),
    /// No `claude` ancestor and no controlling terminal: a daemon, a
    /// launchd job, or a process that detached from its session.
    Detached,
    /// The process table could not be read well enough to decide.
    Unknown(String),
}

impl Origin {
    /// The operator's own terminal or interactive session.
    pub fn is_operator(&self) -> bool {
        matches!(self, Origin::Terminal | Origin::OperatorSession { .. })
    }
}

fn nonempty(env: &BTreeMap<String, String>, key: &str) -> Option<String> {
    env.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn nucleus_of(node: &Node, env: &BTreeMap<String, String>) -> Option<NucleusSession> {
    let kind = nonempty(env, ENV_SESSION)?;
    Some(NucleusSession {
        pid: node.pid,
        kind,
        agent: nonempty(env, crate::agent_msg::ENV_AGENT),
        scope: nonempty(env, crate::caller::ENV_TASK_SCOPE),
        worker: nonempty(env, crate::caller::ENV_TASK_WORKER),
        session_id: node.session_id.clone(),
    })
}

/// Decide the origin from a snapshot. Pure.
///
/// The outermost ancestor whose start environment carries the Nucleus
/// marker (`NUCLEUS_SESSION`) decides, whether or not it is a `claude`
/// process: a Nucleus `claude` carries it, and so does any process started
/// below one that kept its environment (a tmux server a session started,
/// for example). Which ancestors count as `claude` matters only when no
/// ancestor carries the marker, so a process wrongly taken for `claude`
/// cannot hide a marked session below it.
pub fn classify(s: &Snapshot) -> Origin {
    // A chain cut short may hide an outer Nucleus process.
    if !s.complete {
        return Origin::Unknown("the process ancestry could not be read".into());
    }
    let marked = s
        .ancestors
        .iter()
        .enumerate()
        .filter_map(|(i, n)| n.env.as_ref().and_then(|env| nucleus_of(n, env)).map(|m| (i, m)))
        .last();
    // A claude whose environment cannot be read, further out than any
    // marked ancestor, may itself be the outermost Nucleus session.
    let outer_from = marked.as_ref().map(|(i, _)| i + 1).unwrap_or(0);
    if let Some(n) = s.ancestors[outer_from..].iter().find(|n| n.claude && n.env.is_none()) {
        return Origin::Unknown(format!("the environment of the claude process {} could not be read", n.pid));
    }
    if let Some((_, m)) = marked {
        return Origin::Nucleus(m);
    }
    if let Some(top) = s.ancestors.iter().filter(|n| n.claude).last() {
        return Origin::OperatorSession { session_id: top.session_id.clone() };
    }
    let Some(_tty) = &s.tty else {
        return Origin::Detached;
    };
    let mut found: Option<NucleusSession> = None;
    for peer in s.tty_peers.iter().filter(|n| n.claude) {
        let Some(env) = &peer.env else {
            return Origin::Unknown(format!(
                "the environment of the claude process {} on this terminal could not be read",
                peer.pid
            ));
        };
        if let Some(n) = nucleus_of(peer, env) {
            match &found {
                Some(f) if f.kind != n.kind || f.agent != n.agent || f.scope != n.scope || f.worker != n.worker => {
                    return Origin::Unknown(
                        "more than one Nucleus session runs on this terminal".into(),
                    );
                }
                _ => found = Some(n),
            }
        }
    }
    match found {
        Some(n) => Origin::Nucleus(n),
        None => Origin::Terminal,
    }
}

/// Is an executable a `claude` binary? `argv0` and `exec_path` are the
/// process's first argument and the path it was started from. A version
/// directory (`…/claude/versions/2.1.281`) is Claude Code's install layout.
/// Fixed names only: the caller's environment (`NUCLEUS_CLAUDE_BIN`) is not
/// read, since a command could set it to the name of any process above it.
pub fn is_claude_exec(argv0: &str, exec_path: &str) -> bool {
    let base = |p: &str| p.trim_start_matches('-').rsplit('/').next().unwrap_or("").to_string();
    base(argv0) == "claude" || base(exec_path) == "claude" || exec_path.contains("/claude/versions/")
}

/// Session id in a `claude` argument list (`--session-id X`, `--resume X`,
/// or their `=` forms).
pub fn session_id_from_args(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        for flag in ["--session-id", "--resume"] {
            if a == flag {
                return it.next().filter(|v| !v.starts_with('-')).cloned();
            }
            if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
                return Some(v.to_string()).filter(|v| !v.is_empty());
            }
        }
    }
    None
}

/// Keep only the `NUCLEUS_*` variables of an environment list.
fn nucleus_env<'a>(vars: impl Iterator<Item = &'a str>) -> BTreeMap<String, String> {
    vars.filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| k.starts_with("NUCLEUS_"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A process as read from the system.
#[derive(Debug, Clone)]
struct Proc {
    ppid: i32,
    tty: Option<String>,
}

/// Arguments and start environment of a process.
struct Args {
    exec_path: String,
    argv: Vec<String>,
    env: Vec<String>,
}

#[cfg(target_os = "macos")]
mod sys {
    use super::{Args, Proc};

    /// `struct proc_bsdshortinfo` (sys/proc_info.h); the libc crate does not
    /// define it. Readable for every process, including root-owned ones
    /// (`/usr/bin/login` sits in the ancestry of every Terminal shell).
    #[repr(C)]
    #[derive(Default)]
    struct ShortInfo {
        pid: u32,
        ppid: u32,
        pgid: u32,
        status: u32,
        comm: [u8; 16],
        flags: u32,
        uid: u32,
        gid: u32,
        ruid: u32,
        rgid: u32,
        svuid: u32,
        svgid: u32,
        rfu: u32,
    }
    const PROC_PIDT_SHORTBSDINFO: libc::c_int = 13;

    pub fn proc(pid: i32) -> Option<Proc> {
        let mut short = ShortInfo::default();
        let size = std::mem::size_of::<ShortInfo>() as libc::c_int;
        // SAFETY: the buffer is a ShortInfo of the size passed.
        let n = unsafe {
            libc::proc_pidinfo(pid, PROC_PIDT_SHORTBSDINFO, 0, &mut short as *mut _ as *mut libc::c_void, size)
        };
        if n != size {
            return None;
        }
        // The terminal needs the full record, which only the process's own
        // user may read; another user's process has no terminal here.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer is a proc_bsdinfo of the size passed.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        let tty = (n == size && info.e_tdev != 0 && info.e_tdev != u32::MAX).then(|| info.e_tdev.to_string());
        Some(Proc { ppid: short.ppid as i32, tty })
    }

    pub fn all_pids() -> Option<Vec<i32>> {
        // SAFETY: a null buffer asks for the count only.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return None;
        }
        let mut buf = vec![0i32; n as usize + 64];
        let bytes = (buf.len() * std::mem::size_of::<i32>()) as libc::c_int;
        // SAFETY: the buffer holds `bytes` bytes.
        let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr() as *mut libc::c_void, bytes) };
        if got <= 0 {
            return None;
        }
        buf.truncate(got as usize);
        Some(buf.into_iter().filter(|p| *p > 0).collect())
    }

    pub fn args(pid: i32) -> Option<Args> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: a null buffer asks for the size only.
        let rc = unsafe {
            libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
        };
        if rc != 0 || size < 4 {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: the buffer holds `size` bytes.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        buf.truncate(size);
        super::parse_procargs2(&buf)
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use super::{Args, Proc};

    pub fn proc(pid: i32) -> Option<Proc> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the parenthesized command name.
        let rest = &stat[stat.rfind(')')? + 2..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ppid = f.get(1)?.parse().ok()?;
        let tty_nr: i64 = f.get(4)?.parse().ok()?;
        Some(Proc { ppid, tty: (tty_nr != 0).then(|| tty_nr.to_string()) })
    }

    pub fn all_pids() -> Option<Vec<i32>> {
        let rd = std::fs::read_dir("/proc").ok()?;
        Some(rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect())
    }

    pub fn args(pid: i32) -> Option<Args> {
        let split = |b: Vec<u8>| -> Vec<String> {
            b.split(|c| *c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
        };
        let argv = split(std::fs::read(format!("/proc/{pid}/cmdline")).ok()?);
        let env = split(std::fs::read(format!("/proc/{pid}/environ")).ok()?);
        let exec_path = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(Args { exec_path, argv, env })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod sys {
    use super::{Args, Proc};
    pub fn proc(_: i32) -> Option<Proc> {
        None
    }
    pub fn all_pids() -> Option<Vec<i32>> {
        None
    }
    pub fn args(_: i32) -> Option<Args> {
        None
    }
}

/// Parse a `KERN_PROCARGS2` buffer: `argc`, the executable path, padding,
/// `argc` arguments, then the environment up to the first empty string.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_procargs2(buf: &[u8]) -> Option<Args> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let mut pos = 4;
    let next = |pos: &mut usize| -> Option<String> {
        let start = *pos;
        let end = start + buf.get(start..)?.iter().position(|c| *c == 0)?;
        *pos = end + 1;
        Some(String::from_utf8_lossy(&buf[start..end]).into_owned())
    };
    let exec_path = next(&mut pos)?;
    while buf.get(pos) == Some(&0) {
        pos += 1;
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        argv.push(next(&mut pos)?);
    }
    let mut env = Vec::new();
    while pos < buf.len() {
        match next(&mut pos) {
            Some(s) if !s.is_empty() => env.push(s),
            _ => break,
        }
    }
    Some(Args { exec_path, argv, env })
}

fn node_of(pid: i32, args: Option<Args>) -> Node {
    match args {
        Some(a) => {
            let argv0 = a.argv.first().cloned().unwrap_or_default();
            let claude = is_claude_exec(&argv0, &a.exec_path);
            Node {
                pid,
                claude,
                // Every process starts with some environment (HOME, PATH);
                // none means it is hidden (macOS hides it for platform
                // binaries such as /bin/sh).
                env: (!a.env.is_empty()).then(|| nucleus_env(a.env.iter().map(String::as_str))),
                session_id: if claude { session_id_from_args(&a.argv[1.min(a.argv.len())..]) } else { None },
            }
        }
        // Unreadable arguments: another user's process (root, a setuid
        // program). Not a claude process of this user.
        None => Node { pid, claude: false, env: None, session_id: None },
    }
}

/// Read the snapshot of the calling process from the system.
pub fn snapshot() -> Snapshot {
    snapshot_of(std::process::id() as i32)
}

fn snapshot_of(pid: i32) -> Snapshot {
    let me = sys::proc(pid);
    let mut s = Snapshot { tty: me.as_ref().and_then(|p| p.tty.clone()), complete: me.is_some(), ..Default::default() };
    let mut cur = me.map(|p| p.ppid);
    let mut guard = 0;
    while let Some(p) = cur {
        if p <= 1 {
            break;
        }
        guard += 1;
        if guard > 256 {
            s.complete = false;
            break;
        }
        let Some(info) = sys::proc(p) else {
            // The parent exited between reads: the chain above it is gone.
            s.complete = false;
            break;
        };
        s.ancestors.push(node_of(p, sys::args(p)));
        cur = Some(info.ppid);
    }
    let marked = |n: &Node| n.env.as_ref().is_some_and(|e| nonempty(e, ENV_SESSION).is_some());
    if s.ancestors.iter().any(|n| n.claude || marked(n)) {
        return s;
    }
    if let Some(tty) = s.tty.clone() {
        match sys::all_pids() {
            Some(pids) => {
                for p in pids.into_iter().filter(|p| *p != pid) {
                    if sys::proc(p).and_then(|i| i.tty).as_deref() != Some(tty.as_str()) {
                        continue;
                    }
                    let n = node_of(p, sys::args(p));
                    if n.claude {
                        s.tty_peers.push(n);
                    }
                }
            }
            None => s.complete = false,
        }
    }
    s
}

/// The origin of the calling process.
pub fn origin() -> Origin {
    classify(&snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Vector {
        name: String,
        snapshot: Snapshot,
        expect: Expect,
    }

    #[derive(Deserialize)]
    struct Expect {
        origin: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        worker: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    }

    #[test]
    fn shared_vectors() {
        let text = include_str!("../testdata/caller_origin_vectors.json");
        let vectors: Vec<Vector> = serde_json::from_str(text).unwrap();
        assert!(vectors.len() >= 8);
        for v in vectors {
            let got = classify(&v.snapshot);
            let name = &v.name;
            match (v.expect.origin.as_str(), &got) {
                ("terminal", Origin::Terminal) | ("detached", Origin::Detached) | ("unknown", Origin::Unknown(_)) => {}
                ("operator-session", Origin::OperatorSession { session_id }) => {
                    assert_eq!(session_id, &v.expect.session_id, "{name}")
                }
                ("nucleus", Origin::Nucleus(n)) => {
                    assert_eq!(Some(&n.kind), v.expect.kind.as_ref(), "{name}");
                    assert_eq!(n.agent, v.expect.agent, "{name}");
                    assert_eq!(n.scope, v.expect.scope, "{name}");
                    assert_eq!(n.worker, v.expect.worker, "{name}");
                    assert_eq!(n.session_id, v.expect.session_id, "{name}");
                }
                (want, got) => panic!("{name}: expected {want}, got {got:?}"),
            }
        }
    }

    #[test]
    fn procargs2_layout() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/local/bin/claude\0\0\0\0claude\0--resume\0");
        buf.extend_from_slice(b"HOME=/h\0NUCLEUS_SESSION=chat\0NUCLEUS_AGENT=whatsapp\0\0ptr_munge=x\0");
        let a = parse_procargs2(&buf).unwrap();
        assert_eq!(a.exec_path, "/usr/local/bin/claude");
        assert_eq!(a.argv, vec!["claude", "--resume"]);
        assert_eq!(a.env, vec!["HOME=/h", "NUCLEUS_SESSION=chat", "NUCLEUS_AGENT=whatsapp"]);
        let env = nucleus_env(a.env.iter().map(String::as_str));
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn claude_executables_and_session_ids() {
        assert!(is_claude_exec("claude", "/x/.local/bin/claude"));
        assert!(is_claude_exec("-claude", ""));
        assert!(is_claude_exec("node", "/x/.local/share/claude/versions/2.1.281"));
        assert!(!is_claude_exec("zsh", "/bin/zsh"));
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(session_id_from_args(&args(&["--session-id", "abc"])).as_deref(), Some("abc"));
        assert_eq!(session_id_from_args(&args(&["--model", "m", "--resume", "def"])).as_deref(), Some("def"));
        assert_eq!(session_id_from_args(&args(&["--resume=ghi"])).as_deref(), Some("ghi"));
        assert_eq!(session_id_from_args(&args(&["--resume"])), None);
    }

    /// The live snapshot of the test process: its chain ends at the root,
    /// and whatever runs the tests (a terminal, an operator session, CI) is
    /// never a Nucleus session unless the test runs inside one.
    #[test]
    fn live_snapshot_reads_the_ancestry() {
        let s = snapshot();
        assert!(s.complete, "{s:?}");
        assert!(!s.ancestors.is_empty());
    }
}
