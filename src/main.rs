mod agents;
mod mcp;
mod model;
mod ops;
mod store;

use agents::{ReadQuery, WaitTarget};
use clap::{Args, Parser, Subcommand};
use model::{Approval, Caller, CallerKind, Outcome, Receipt, Turn};
use ops::{
    DEFAULT_TIMEOUT, LsArgs, Models, ModelsArgs, NewArgs, Output, Read, ReadArgs, Request,
    SendArgs, Sessions, WaitArgs, caller_from_handle, env_var,
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
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Who is sending.
#[derive(Args)]
struct SenderOpts {
    /// Handle of the session sending this; default: the session the shell runs in, if
    /// known.
    #[arg(long)]
    from: Option<String>,
}

#[derive(Args)]
struct WaitOpts {
    /// Wait for the reply and print the finished turn; the reply is its final_text (on
    /// OpenCode the reply of the run that took the message, which may also cover other
    /// queued messages; read shows them). Claude, Grok and Antigravity run the whole turn
    /// before returning either way.
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
    /// Show each agent's installed version and which operations work now.
    Status,
    /// List the models an agent can start a session on, with the effort values each
    /// takes; one page per call, sorted by id.
    Models {
        /// codex, claude, opencode, grok, antigravity or pi.
        agent: String,
        /// Only models whose id contains this text.
        query: Option<String>,
        /// Models per page.
        #[arg(long, default_value_t = ops::MODELS_LIMIT)]
        limit: usize,
        /// Cursor printed by a previous page.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// List sessions, one page per call; pass a handle to send or read.
    Ls {
        /// Only this agent: codex, claude, opencode, grok, antigravity or pi.
        #[arg(long)]
        agent: Option<String>,
        /// Only sessions in this working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Include sub-agent sessions and sessions of unknown origin.
        #[arg(long)]
        all: bool,
        /// Sessions per page.
        #[arg(long, default_value_t = ops::LS_LIMIT)]
        limit: u32,
        /// Cursor printed by a previous page; needs --agent.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Start a session with a first message; prints its handle and a receipt.
    New {
        /// codex, claude, opencode, grok, antigravity or pi.
        agent: String,
        /// The first message.
        prompt: String,
        /// Working directory of the session.
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// Model id as listed by models; default: the agent's.
        #[arg(long)]
        model: Option<String>,
        /// Title shown by ls; Antigravity has none.
        #[arg(long)]
        name: Option<String>,
        /// Reasoning effort, one of the model's values listed by models; default: the
        /// agent's.
        #[arg(long)]
        effort: Option<String>,
        /// Let the session act without asking for permission; default: the agent's own
        /// permissions.
        #[arg(long)]
        full_access: bool,
        #[command(flatten)]
        wait: WaitOpts,
        #[command(flatten)]
        sender: SenderOpts,
    },
    /// Send a message to a session; returns once it is accepted, or with --wait once the
    /// reply is in.
    Send {
        /// Session handle from ls or new.
        handle: String,
        /// The message.
        text: String,
        /// Deliver into the running turn instead of after it; refused when the session is
        /// idle.
        #[arg(long)]
        steer: bool,
        #[command(flatten)]
        wait: WaitOpts,
        #[command(flatten)]
        sender: SenderOpts,
    },
    /// Read a session's newest messages, oldest first.
    Read {
        /// Session handle.
        handle: String,
        /// Messages per page.
        #[arg(long, default_value_t = ops::READ_LIMIT)]
        limit: usize,
        /// Cursor printed by a previous page, for the messages before it.
        #[arg(long)]
        cursor: Option<String>,
        /// Every message, including intermediate text and tool calls; by default only what
        /// was sent and the final replies.
        #[arg(long)]
        all: bool,
        /// Print the agent's raw records of the span instead of messages.
        #[arg(long)]
        raw: bool,
    },
    /// Wait for a turn to finish and print it; the reply is its final_text.
    Wait {
        /// Session handle.
        handle: String,
        /// Turn id.
        #[arg(long, required_unless_present = "receipt", conflicts_with = "receipt")]
        turn: Option<String>,
        /// Receipt id from a send or new.
        #[arg(long)]
        receipt: Option<String>,
        /// Seconds to wait.
        #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs())]
        timeout: u64,
    },
    /// Serve the same commands as MCP tools on stdio.
    Mcp {
        /// Attribute every send and new to this handle; default: the session that started
        /// this server, if known.
        #[arg(long)]
        caller: Option<String>,
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
    if let Cmd::Mcp { caller } = cli.cmd {
        return match rt.block_on(mcp::serve(caller)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(e.code.exit_code())
            }
        };
    }
    agents::interrupt_on_signals();
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
                    eprintln!("handle: {}", r.handle);
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
        Cmd::Status => Request::Status {
            sender: cli_caller(None)?,
        },
        Cmd::Mcp { .. } => unreachable!("handled in main"),
        Cmd::Models {
            agent,
            query,
            limit,
            cursor,
        } => Request::Models(ModelsArgs {
            agent,
            query,
            limit,
            cursor,
        }),
        Cmd::Ls {
            agent,
            cwd,
            all,
            limit,
            cursor,
        } => Request::Ls(LsArgs {
            agent,
            cwd,
            all,
            limit,
            cursor,
        }),
        Cmd::New {
            agent,
            prompt,
            cwd,
            model,
            name,
            effort,
            full_access,
            wait,
            sender,
        } => Request::New(NewArgs {
            agent,
            cwd,
            prompt,
            model,
            name,
            effort,
            full_access,
            wait: wait.duration(),
            from: cli_caller(sender.from.as_deref())?,
        }),
        Cmd::Send {
            handle,
            text,
            steer,
            wait,
            sender,
        } => Request::Send(SendArgs {
            handle,
            text,
            steer,
            wait: wait.duration(),
            from: cli_caller(sender.from.as_deref())?,
        }),
        Cmd::Read {
            handle,
            limit,
            cursor,
            all,
            raw,
        } => Request::Read(ReadArgs {
            handle,
            query: ReadQuery {
                limit,
                before: cursor,
                all,
            },
            raw,
        }),
        Cmd::Wait {
            handle,
            turn,
            receipt,
            timeout,
        } => Request::Wait(WaitArgs {
            handle,
            target: match (turn, receipt) {
                (Some(t), _) => WaitTarget::Turn(t),
                (None, Some(r)) => WaitTarget::Receipt(r),
                (None, None) => unreachable!("clap requires --turn or --receipt"),
            },
            timeout: Duration::from_secs(timeout),
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
        Output::Status { agents, sender } => {
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
        Output::Models(Models {
            models,
            next_cursor,
        }) => {
            for m in models {
                println!("{:<48} {}", m.id, m.efforts.join(" "));
            }
            if let Some(c) = next_cursor {
                println!("next cursor: {c}");
            }
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
