mod agents;
mod mcp;
mod model;
mod ops;
mod store;

use agents::{ApprovalPolicy, Mode, ReadRange, WaitTarget};
use clap::{Args, Parser, Subcommand};
use model::{Approval, Caller, CallerKind, Outcome, Receipt, Turn};
use ops::{
    DEFAULT_MAX_HOPS, DEFAULT_TIMEOUT, LsArgs, NewArgs, Output, Read, ReadArgs, Request, SendArgs,
    Sessions, WaitArgs, caller_from_handle, env_var,
};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use store::Store;

#[derive(Parser)]
#[command(
    name = "agent-talk",
    version,
    about = "Send messages to agent sessions and read the replies"
)]
struct Cli {
    /// Print machine-readable JSON on stdout.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Who is sending, and how far an agent-to-agent chain may go.
#[derive(Args)]
struct SenderOpts {
    /// Handle of the session issuing this command. Default: AGENT_TALK_CALLER, else the
    /// session the shell runs in (CODEX_THREAD_ID, OPENCODE_SESSION_ID, GROK_SESSION_ID,
    /// ANTIGRAVITY_CONVERSATION_ID, then CLAUDE_CODE_SESSION_ID), else unknown.
    #[arg(long)]
    from: Option<String>,
    /// Refuse when the message's hop depth exceeds this: 1 + the depth of the message
    /// it answers (--reply-to, else for an agent sender the message that started its
    /// turn or the newest one delivered to it); 0 for an unknown sender without --reply-to.
    #[arg(long, default_value_t = DEFAULT_MAX_HOPS)]
    max_hops: u32,
}

#[derive(Args)]
struct WaitOpts {
    /// Wait for the turn that consumes this message to complete and print it. Its
    /// final_text is the reply to this message on Codex, Claude, Grok and Antigravity; on
    /// OpenCode it is the final reply of the execution that consumed it, which may also have
    /// answered other messages queued meanwhile (read --tail shows the adjacent messages).
    #[arg(long)]
    wait: bool,
    /// Seconds to wait with --wait.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs())]
    timeout: u64,
}

impl WaitOpts {
    fn duration(&self) -> Option<Duration> {
        self.wait.then(|| Duration::from_secs(self.timeout))
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// What each agent can do now: CLI version, daemon, service or leader, available operations.
    Caps,
    /// List sessions with observations, paginated.
    Ls {
        /// codex, claude, opencode, grok or antigravity; default: every agent's first page.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Include every source kind (sub-agents, unknown).
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = ops::LS_LIMIT)]
        limit: u32,
        /// Cursor from a previous page of the same agent.
        #[arg(long)]
        cursor: Option<String>,
        /// Include the agent's raw records under `raw` in --json output (large).
        #[arg(long)]
        raw: bool,
    },
    /// Start a session owned by agent-talk; prints handle and receipt.
    New {
        /// codex, claude, opencode, grok or antigravity.
        agent: String,
        prompt: String,
        #[arg(long)]
        cwd: PathBuf,
        /// Model for the session (Codex: model id; Claude: e.g. sonnet; OpenCode:
        /// provider/model[#variant]; Grok: e.g. grok-4.7; Antigravity: e.g. gemini-3.8-flash).
        #[arg(long)]
        model: Option<String>,
        /// Title stored with the session by the agent, shown by ls (Codex thread name,
        /// Claude session name, OpenCode title, Grok session title; Antigravity has none).
        #[arg(long)]
        name: Option<String>,
        /// Reasoning effort for the session (Codex, Grok, Antigravity; the model's values,
        /// e.g. low | medium | high); default: the agent's.
        #[arg(long)]
        effort: Option<String>,
        /// Agentic turn limit for the first turn (Claude only).
        #[arg(long)]
        max_turns: Option<u32>,
        /// Approval policy for the new thread (Codex only, e.g. on-request); default: the
        /// daemon's.
        #[arg(long)]
        approval_policy: Option<String>,
        /// Sandbox for the new thread (Codex only): read-only | workspace-write |
        /// danger-full-access; default: the daemon's.
        #[arg(long)]
        sandbox: Option<String>,
        /// Approval requests reaching agent-talk while it observes the turn.
        #[arg(long, value_enum)]
        approvals: Option<ApprovalPolicy>,
        /// Include the agent's raw record of the turn under `raw` in --json output (large).
        #[arg(long)]
        raw: bool,
        #[command(flatten)]
        wait: WaitOpts,
        #[command(flatten)]
        sender: SenderOpts,
    },
    /// Send a message to a session. Claude, Grok and Antigravity sends are synchronous: the
    /// turn runs to completion even without --wait, which only controls whether the turn is
    /// printed (Grok without a leader keeps no process after the command, so a --wait
    /// deadline or Ctrl-C ends the turn with session/cancel).
    Send {
        handle: String,
        text: String,
        /// queue: after the current reply (Codex, Grok: as its own turn; OpenCode: inside the
        /// same execution; Antigravity: as the next turn of a new agy process); steer: into
        /// the running turn (Codex, OpenCode, Grok through a live leader), refused when the
        /// session is idle.
        #[arg(long, value_enum, default_value_t = Mode::Queue)]
        mode: Mode,
        /// Receipt, turn or message id this message answers.
        #[arg(long)]
        reply_to: Option<String>,
        /// Steer only: the running turn id you expect (default: the newest turn).
        #[arg(long)]
        expect_turn: Option<String>,
        /// Model for the turn this message starts (Claude, Antigravity). Antigravity otherwise
        /// reuses the model, and always the effort, that `new` recorded.
        #[arg(long)]
        model: Option<String>,
        /// Agentic turn limit for the turn this message starts (Claude only).
        #[arg(long)]
        max_turns: Option<u32>,
        /// Approval requests reaching agent-talk while it observes the turn.
        #[arg(long, value_enum)]
        approvals: Option<ApprovalPolicy>,
        /// Include the agent's raw record of the turn under `raw` in --json output (large).
        #[arg(long)]
        raw: bool,
        #[command(flatten)]
        wait: WaitOpts,
        #[command(flatten)]
        sender: SenderOpts,
    },
    /// Read messages, oldest first, one page per call.
    Read {
        handle: String,
        /// Cursor from a previous page.
        #[arg(long, conflicts_with = "tail")]
        since: Option<String>,
        /// Page size, oldest first (Codex: turns; OpenCode: message rows; Claude, Grok,
        /// Antigravity: messages).
        #[arg(long, default_value_t = ops::READ_LIMIT, conflicts_with = "tail")]
        limit: u32,
        /// The newest N messages instead, printed oldest first; no cursor.
        #[arg(long)]
        tail: Option<u32>,
        /// Print the agent's raw records of the span instead of normalized messages.
        #[arg(long)]
        raw: bool,
    },
    /// Wait for a specific turn, or the turn that consumed a receipt, to complete. On
    /// OpenCode a turn is one execution and may have answered several messages.
    Wait {
        handle: String,
        #[arg(long, required_unless_present = "receipt", conflicts_with = "receipt")]
        turn: Option<String>,
        #[arg(long)]
        receipt: Option<String>,
        #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs())]
        timeout: u64,
        /// Approval requests reaching agent-talk while it observes the turn.
        #[arg(long, value_enum)]
        approvals: Option<ApprovalPolicy>,
        /// Include the agent's raw record of the turn under `raw` in --json output (large).
        #[arg(long)]
        raw: bool,
    },
    /// Serve ls, new, send, read and wait as MCP tools on stdio.
    Mcp {
        /// Pin the sender of every send and new to this handle. Without it: AGENT_TALK_CALLER
        /// (also a pin, but inherited by child processes), then each call's
        /// `_meta.threadId` (Codex), `_meta["ai.opencode/sessionID"]` (OpenCode) or
        /// `_meta["antigravity.google/conversation_id"]` (Antigravity), then GROK_SESSION_ID
        /// or CLAUDE_CODE_SESSION_ID in this server's environment (set by Grok or Claude Code
        /// for each MCP server they start), then unknown. None of these is authenticated.
        #[arg(long)]
        caller: Option<String>,
        /// Hop limit applied to every send and new issued through this server.
        #[arg(long, default_value_t = DEFAULT_MAX_HOPS)]
        max_hops: u32,
    },
}

fn main() -> ExitCode {
    // Die quietly when the reader of stdout goes away (`agent-talk ls --json | head`), like
    // any Unix tool, instead of panicking on EPIPE. Agent connections are unaffected:
    // std, mio and socket2 set SO_NOSIGPIPE on every socket they open on macOS.
    // SAFETY: called before any thread exists; SIG_DFL is a valid disposition.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AGENT_TALK_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let json = cli.json;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Cmd::Mcp { caller, max_hops } = cli.cmd {
        return match rt.block_on(mcp::serve(caller, max_hops)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(e.code.exit_code())
            }
        };
    }
    let res = rt.block_on(async {
        let store = Store::open()?;
        ops::run(&store, request(cli.cmd)?).await
    });
    match res {
        Ok(out) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else {
                print_human(&out);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "error": e })).unwrap()
                );
            } else {
                eprintln!("error: {e}");
                if let Some(state) = e.state {
                    eprintln!("state: {state}");
                }
                if let Some(v) = &e.agent_error {
                    eprintln!("agent error: {} {}", v.code, v.message);
                }
                if let Some(r) = &e.receipt {
                    eprintln!("receipt: {}", receipt_line(r));
                }
                for a in &e.approvals {
                    eprintln!("{}", approval_line(a));
                }
            }
            ExitCode::from(e.code.exit_code())
        }
    }
}

/// Sender of a CLI mutation (DESIGN.md §4).
fn cli_caller(explicit: Option<&str>) -> model::Result<Caller> {
    if let Some(h) = explicit
        .map(String::from)
        .or_else(|| env_var("AGENT_TALK_CALLER"))
    {
        return caller_from_handle(&h);
    }
    for (var, agent) in agents::SESSION_VARS {
        if let Some(id) = env_var(var) {
            return Ok(Caller::agent(&format!("{agent}:{id}")));
        }
    }
    Ok(Caller::UNKNOWN)
}

fn request(cmd: Cmd) -> model::Result<Request> {
    Ok(match cmd {
        Cmd::Caps => Request::Caps {
            sender: cli_caller(None)?,
        },
        Cmd::Mcp { .. } => unreachable!("handled in main"),
        Cmd::Ls {
            agent,
            cwd,
            all,
            limit,
            cursor,
            raw,
        } => Request::Ls(LsArgs {
            agent,
            cwd,
            all,
            limit,
            cursor,
            raw,
        }),
        Cmd::New {
            agent,
            prompt,
            cwd,
            model,
            name,
            effort,
            max_turns,
            approval_policy,
            sandbox,
            approvals,
            raw,
            wait,
            sender,
        } => Request::New(NewArgs {
            agent,
            cwd,
            prompt,
            model,
            name,
            effort,
            max_turns,
            approval_policy,
            sandbox,
            approvals,
            wait: wait.duration(),
            from: cli_caller(sender.from.as_deref())?,
            max_hops: sender.max_hops,
            raw,
        }),
        Cmd::Send {
            handle,
            text,
            mode,
            reply_to,
            expect_turn,
            model,
            max_turns,
            approvals,
            raw,
            wait,
            sender,
        } => Request::Send(SendArgs {
            handle,
            text,
            mode,
            expect_turn,
            reply_to,
            model,
            max_turns,
            approvals,
            wait: wait.duration(),
            from: cli_caller(sender.from.as_deref())?,
            max_hops: sender.max_hops,
            raw,
        }),
        Cmd::Read {
            handle,
            since,
            limit,
            tail,
            raw,
        } => Request::Read(ReadArgs {
            handle,
            range: match tail {
                Some(n) => ReadRange::Tail(n),
                None => ReadRange::Forward { since, limit },
            },
            raw,
        }),
        Cmd::Wait {
            handle,
            turn,
            receipt,
            timeout,
            approvals,
            raw,
        } => Request::Wait(WaitArgs {
            handle,
            target: match (turn, receipt) {
                (Some(t), _) => WaitTarget::Turn(t),
                (None, Some(r)) => WaitTarget::Receipt(r),
                (None, None) => unreachable!("clap requires --turn or --receipt"),
            },
            approvals,
            timeout: Duration::from_secs(timeout),
            raw,
        }),
    })
}

fn or_dash(v: Option<&str>) -> &str {
    v.unwrap_or("-")
}

fn receipt_line(r: &Receipt) -> String {
    format!(
        "{} state={} queue={} turn={}",
        r.receipt_id,
        r.state.as_str(),
        or_dash(r.queue_id.as_deref()),
        or_dash(r.turn_id.as_deref())
    )
}

fn approval_line(a: &Approval) -> String {
    format!("approval {}: {} ({})", a.outcome, a.summary, a.kind)
}

fn print_human(out: &Output) {
    match out {
        Output::Caps { agents, sender } => {
            for p in agents {
                println!(
                    "{:<12} {}",
                    p.agent,
                    p.version.as_deref().unwrap_or("version unknown")
                );
                if let Some(shared) = &p.shared {
                    println!("{:<12} {shared}", "");
                }
                let available: Vec<&str> = p
                    .operations
                    .iter()
                    .filter(|o| o.available)
                    .map(|o| o.name)
                    .collect();
                if !available.is_empty() {
                    println!("{:<12} available: {}", "", available.join(" "));
                }
                // Operations blocked for the same reason share one line.
                let mut blocked: Vec<(&str, Vec<&str>)> = Vec::new();
                for o in p.operations.iter().filter(|o| !o.available) {
                    let reason = o.reason.as_deref().unwrap_or("unavailable");
                    match blocked.iter_mut().find(|(r, _)| *r == reason) {
                        Some((_, names)) => names.push(o.name),
                        None => blocked.push((reason, vec![o.name])),
                    }
                }
                for (reason, names) in blocked {
                    println!("{:<12} {}: {reason}", "", names.join(", "));
                }
            }
            println!(
                "{:<12} {}",
                "sender",
                sender.session.as_deref().unwrap_or("unknown")
            );
        }
        Output::Sessions(Sessions {
            sessions,
            next_cursors,
            errors,
        }) => {
            print_sessions(sessions);
            for (p, c) in next_cursors {
                if let Some(c) = c {
                    println!("next cursor ({p}): {c}");
                }
            }
            for e in errors {
                println!("{}: {}", e.agent, e.error.message);
            }
        }
        Output::Read(Read {
            messages,
            raw,
            next_cursor,
            ..
        }) => {
            for m in messages.iter().flatten() {
                let time = m
                    .timestamp
                    .as_deref()
                    .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                    .map(|t| {
                        t.to_zoned(jiff::tz::TimeZone::system())
                            .strftime("%H:%M:%S ")
                            .to_string()
                    })
                    .unwrap_or_default();
                let label = match (m.role, m.from.as_ref()) {
                    ("user", Some(from)) => match (from.kind, &from.session) {
                        (CallerKind::Runtime, _) => "user runtime".to_string(),
                        (CallerKind::Agent, Some(h)) => format!("user from {h}"),
                        _ => "user".to_string(),
                    },
                    ("user", None) => "user".to_string(),
                    (role, _) => format!("{role} {}", m.phase),
                };
                println!("{time}[{label}] {}", m.text);
            }
            if let Some(rows) = raw {
                println!("{}", serde_json::to_string_pretty(rows).unwrap());
            }
            if let Some(c) = next_cursor {
                println!("next cursor: {c}");
            }
        }
        Output::Outcome(o) => print_outcome(o),
    }
}

fn print_sessions(sessions: &[model::Session]) {
    for x in sessions {
        let label = x
            .name
            .as_deref()
            .or(x.preview.as_deref())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        println!(
            "{}  {:<7} loaded={:<3} origin={:<10} {}  {}",
            x.handle,
            x.state,
            x.observations.loaded,
            x.observations.origin,
            or_dash(x.cwd.as_deref()),
            label.chars().take(60).collect::<String>()
        );
    }
}

fn print_outcome(o: &Outcome) {
    println!("handle   {}", o.handle);
    if let Some(from) = &o.from {
        println!(
            "from     {} {}",
            match from.kind {
                CallerKind::Agent => "agent",
                CallerKind::Unknown => "unknown",
                CallerKind::Runtime => "runtime",
            },
            from.session.as_deref().unwrap_or("")
        );
    }
    if let Some(r) = &o.receipt {
        println!("receipt  {}", receipt_line(r));
    }
    if let Some(Turn {
        turn_id,
        status,
        basis,
        final_text,
        ..
    }) = &o.turn
    {
        println!("turn     {turn_id} status={status}");
        if let Some(b) = basis {
            println!("basis    {b}");
        }
        if let Some(text) = final_text {
            println!("{text}");
        }
    }
    for a in &o.approvals {
        println!("{}", approval_line(a));
    }
}
