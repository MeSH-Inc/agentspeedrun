//! `speedrun`: CLI, host hook adapter and MCP server for agentspeedrun.

mod client;
mod hook;
mod mcp;
mod ops;
mod project;
mod redacted;
mod secret;
mod setup;
mod tools;
mod watch;

use std::io::Read;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;
use speedrun_core::{
    EventPayload, MilestoneOrigin, RunOutcome, Trust,
    api::{CategoriesResponse, ChainReport, RotateResponse},
    category::DEFAULT_CATEGORY,
};

use client::Client;
use project::Project;

#[derive(Parser)]
#[command(
    name = "speedrun",
    version,
    about = "Time milestones of agentic coding projects"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Read the value from the terminal (hidden) or stdin.
    Set {
        name: secret::Name,
    },
    Rm {
        name: secret::Name,
    },
    List,
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Replace this device's signing key. The daemon certifies the new key
    /// with the old one, so runs signed before the rotation still verify.
    Rotate,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start tracking this repository. The run's timer starts at the next prompt.
    Init {
        #[arg(long, default_value = DEFAULT_CATEGORY)]
        category: String,
        #[arg(long)]
        force: bool,
    },
    /// Show the current run.
    Status {
        #[arg(long, conflicts_with = "line")]
        json: bool,
        /// One line for status bars; prints nothing when there is no run.
        #[arg(long)]
        line: bool,
    },
    /// Mark a milestone as reached. The daemon checks attached evidence and
    /// upgrades the claim to evidence_verified when it holds up.
    Split {
        /// Milestone id or category key.
        milestone: String,
        /// Commit that achieves it (SHA or ref such as HEAD).
        #[arg(long)]
        commit: Option<String>,
        /// Public HTTPS page: the deployment, or a terms/privacy page. Repeatable.
        #[arg(long)]
        url: Vec<String>,
        /// GitHub Actions run URL that passed on this project's history.
        #[arg(long)]
        ci: Option<String>,
        /// Stripe payment, charge, checkout session, invoice or event id
        /// (needs `speedrun secret set stripe`).
        #[arg(long)]
        stripe: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Add your own milestone (accepted immediately).
    Add {
        title: String,
        #[arg(long)]
        description: Option<String>,
    },
    /// Accept an agent-proposed milestone.
    Accept {
        milestone: String,
        /// Change the trust the milestone requires.
        #[arg(long, value_parser = ["self_declared", "agent_claimed", "evidence_verified"])]
        required_trust: Option<String>,
    },
    /// Reject an agent-proposed milestone.
    Reject {
        milestone: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Undo a split.
    Invalidate {
        milestone: String,
        #[arg(long)]
        reason: String,
    },
    Pause {
        #[arg(long)]
        reason: Option<String>,
    },
    Resume,
    /// Close the current run (abandoning it if unfinished); the next prompt starts a new one.
    Reset {
        #[arg(long)]
        yes: bool,
    },
    /// Print the run's raw event log.
    Log,
    /// Check the run's hash chain.
    Verify,
    /// Print a run's tier 2 disclosure export: per-prompt commitments, no prompt text.
    Export {
        /// Disclosure tier to export (only 2 is defined).
        #[arg(long, default_value_t = 2)]
        tier: u8,
    },
    /// List available categories.
    Categories,
    /// Deliver events spooled while the daemon was unreachable.
    Flush,
    /// Entry point for host hooks, e.g. `speedrun hook claude-code user-prompt-submit`.
    Hook {
        host: String,
        event: String,
    },
    /// Generic adapter for hosts without a dedicated hook (opencode, pi, editors).
    /// Reads the prompt text from stdin for `prompt`. Prints one JSON object:
    /// `{recorded, spooled, run_started, run_id, context}`. Always exits 0.
    Ingest {
        #[arg(value_parser = ["prompt", "turn-end"])]
        kind: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        session: String,
        #[arg(long)]
        model: Option<String>,
    },
    /// Live view of all active runs, ticking locally from the daemon's stream.
    Watch {
        /// Only runs active in the last this-many hours.
        #[arg(long, default_value_t = 168)]
        since_hours: u32,
        /// Print one frame and exit.
        #[arg(long)]
        once: bool,
    },
    /// Print the read-only display token and live endpoints for widgets.
    Display {
        /// `{"base_url", "token"}` for programs.
        #[arg(long)]
        json: bool,
    },
    /// Run an agent tool (as exposed over MCP) and print its text result.
    /// Exits 1 if the tool failed. `--list` prints the tool definitions.
    Tool {
        #[arg(required_unless_present = "list")]
        name: Option<String>,
        /// Tool arguments as a JSON object.
        #[arg(long, default_value = "{}")]
        args: String,
        #[arg(long)]
        list: bool,
    },
    /// Install the adapter for opencode, pi, oh-my-pi or Copilot agent hooks
    /// (VS Code agent, Copilot CLI): user-wide, or for this project with
    /// --project. Claude Code uses its plugin marketplace.
    Setup {
        host: setup::Host,
        #[arg(long)]
        project: bool,
        /// Replace an existing file speedrun did not write.
        #[arg(long)]
        force: bool,
    },
    /// Run the MCP server on stdio.
    Mcp {
        /// Project to serve. Defaults to the client's MCP roots, then the working directory.
        #[arg(long)]
        project_dir: Option<std::path::PathBuf>,
    },
    /// Print the JSON Schema of the plugin ↔ daemon protocol.
    Schema,
    /// Store credentials the daemon's verifiers use (Stripe, GitHub).
    Secret {
        #[command(subcommand)]
        action: SecretCmd,
    },
    /// Manage this device's signing key.
    Key {
        #[command(subcommand)]
        action: KeyCmd,
    },
}

/// Command-line parsing at the process boundary.
///
/// One decision varies: host adapters (`hook`, `ingest`) are called by editors and
/// agents that treat a non-zero exit as a blocking failure, so their argument errors
/// are swallowed. Every other subcommand keeps clap's strict parsing.
mod invocation {
    use std::ffi::OsString;

    use clap::Parser;

    use super::Cli;

    /// Subcommands whose argv comes from an editor or agent host.
    const HOST_COMMANDS: [&str; 2] = ["hook", "ingest"];

    /// Flags `ingest` understands; anything else may come from a newer adapter.
    const INGEST_FLAGS: [&str; 3] = ["--host", "--session", "--model"];

    /// Parse argv, returning `None` when a host adapter's command line is unusable.
    ///
    /// `None` means "exit successfully without dispatching": the host must never be
    /// blocked by an argument speedrun did not understand.
    pub(super) fn parse() -> Option<Cli> {
        let args: Vec<OsString> = std::env::args_os().collect();
        if !is_host_command(&args) {
            return Some(Cli::parse());
        }
        match Cli::try_parse_from(relaxed(&args)) {
            Ok(cli) => Some(cli),
            Err(err) => {
                report(&err);
                // `ingest` adapters parse stdout, so it still prints its one JSON object.
                if args.get(1).and_then(|arg| arg.to_str()) == Some("ingest") {
                    let reply = serde_json::json!({
                        "recorded": false,
                        "error": format!("unusable arguments: {}", err.kind()),
                    });
                    println!("{reply}");
                }
                None
            }
        }
    }

    fn is_host_command(args: &[OsString]) -> bool {
        args.get(1)
            .and_then(|arg| arg.to_str())
            .is_some_and(|name| HOST_COMMANDS.contains(&name))
    }

    /// Drop arguments a newer host plugin may have added.
    fn relaxed(args: &[OsString]) -> Vec<OsString> {
        match args.get(1).and_then(|arg| arg.to_str()) {
            // `hook <host> <event>`: whatever follows the event is optional for speedrun.
            Some("hook") => args.iter().take(4).cloned().collect(),
            // `ingest <kind> --host .. --session .. [--model ..]`: keep the fields we know
            // and discard unknown flags instead of failing the adapter.
            Some("ingest") => args
                .iter()
                .filter(|arg| match arg.to_str() {
                    Some(arg) => !arg.starts_with('-') || is_ingest_flag(arg),
                    None => true,
                })
                .cloned()
                .collect(),
            _ => args.to_vec(),
        }
    }

    fn is_ingest_flag(arg: &str) -> bool {
        let name = arg.split_once('=').map_or(arg, |(name, _)| name);
        INGEST_FLAGS.contains(&name)
    }

    /// Argument errors are noise for hosts unless someone is debugging the adapter.
    fn report(err: &clap::Error) {
        if std::env::var_os("SPEEDRUN_DEBUG").is_some() {
            eprintln!("speedrun: ignoring host adapter arguments: {err}");
        }
    }
}

fn main() {
    // `speedrun status | head` closes stdout early; stop quietly instead of panicking.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or_default();
        if msg.starts_with("failed printing to stdout") && msg.contains("Broken pipe") {
            std::process::exit(0);
        }
        default_hook(info);
    }));
    // Host adapters own their own argv policy; `None` means their command line was
    // unusable and the host must not see a failure.
    let Some(cli) = invocation::parse() else {
        return;
    };
    // Hooks must never fail the host.
    if let Cmd::Hook { host, event } = &cli.command {
        match host.as_str() {
            "claude-code" => hook::claude_code(event),
            "copilot" => hook::copilot(event),
            other => eprintln!("speedrun: unknown hook host {other}"),
        }
        return;
    }
    if let Err(e) = run(cli.command) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cmd: Cmd) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let c = Client::new();
    match cmd {
        Cmd::Init { category, force } => {
            let cats: CategoriesResponse = c.get("/v1/categories")?;
            if !cats.categories.iter().any(|k| k.id == category) {
                let ids: Vec<_> = cats.categories.iter().map(|k| k.id.as_str()).collect();
                bail!("unknown category {category}; available: {}", ids.join(", "));
            }
            let p = Project::init(&cwd, &category, force)?;
            println!(
                "Tracking {} as {} ({}).\nCommit .speedrun/project.json. The timer starts at the next prompt.",
                p.root.display(),
                p.config.project_id,
                category
            );
        }
        Cmd::Status { json, line } => {
            let p = Project::require(&cwd)?;
            match ops::active_run(&c, &p)? {
                Some(run) if json => println!("{}", serde_json::to_string_pretty(&run)?),
                Some(run) if line => println!("{}", ops::format_line(&run)),
                None if line => {}
                Some(run) => print!("{}", ops::format_status(&run)),
                None => println!("No active run; the timer starts at the next prompt."),
            }
        }
        Cmd::Split {
            milestone,
            commit,
            url,
            ci,
            stripe,
            note,
        } => {
            let p = Project::require(&cwd)?;
            let run = ops::require_run(&c, &p)?;
            let milestone_id = ops::milestone_id(&run, &milestone)?;
            let evidence = ops::Claim {
                commit,
                urls: url,
                ci,
                stripe,
                note,
            }
            .evidence(&p)?;
            let resp = ops::append_now(
                &Client::patient(),
                &p,
                EventPayload::MilestoneReached {
                    milestone_id,
                    trust: Trust::SelfDeclared,
                    evidence,
                },
            )?;
            if let Some(v) = &resp.verification {
                println!("{}\n", ops::format_verification(v));
            }
            print!("{}", ops::format_status(&ops::require_run(&c, &p)?));
        }
        Cmd::Add { title, description } => {
            let p = Project::require(&cwd)?;
            let milestone_id = speedrun_core::new_id();
            ops::append_now(
                &c,
                &p,
                EventPayload::MilestoneProposed {
                    milestone_id: milestone_id.clone(),
                    origin: MilestoneOrigin::User,
                    key: None,
                    title,
                    description,
                    required_trust: Trust::SelfDeclared,
                },
            )?;
            ops::append_now(
                &c,
                &p,
                EventPayload::MilestoneAccepted {
                    milestone_id: milestone_id.clone(),
                    required_trust: None,
                },
            )?;
            println!("Added {milestone_id}");
        }
        Cmd::Accept {
            milestone,
            required_trust,
        } => {
            let required_trust = required_trust
                .map(|t| serde_json::from_value::<Trust>(serde_json::Value::String(t)))
                .transpose()?;
            milestone_event(&c, &cwd, &milestone, |milestone_id| {
                EventPayload::MilestoneAccepted {
                    milestone_id,
                    required_trust,
                }
            })?
        }
        Cmd::Reject { milestone, reason } => milestone_event(&c, &cwd, &milestone, |milestone_id| {
            EventPayload::MilestoneRejected { milestone_id, reason }
        })?,
        Cmd::Invalidate { milestone, reason } => milestone_event(&c, &cwd, &milestone, |milestone_id| {
            EventPayload::MilestoneInvalidated { milestone_id, reason }
        })?,
        Cmd::Pause { reason } => {
            ops::append_now(&c, &Project::require(&cwd)?, EventPayload::RunPaused { reason })?;
        }
        Cmd::Resume => {
            ops::append_now(&c, &Project::require(&cwd)?, EventPayload::RunResumed {})?;
        }
        Cmd::Reset { yes } => {
            if !yes {
                bail!("this closes the current run permanently; re-run with --yes");
            }
            ops::append_now(
                &c,
                &Project::require(&cwd)?,
                EventPayload::RunFinished {
                    outcome: RunOutcome::Abandoned,
                    reason: Some("reset by user".into()),
                },
            )?;
            println!("Run closed. The next prompt starts a new run.");
        }
        Cmd::Log => {
            let p = Project::require(&cwd)?;
            let run = ops::require_run(&c, &p)?;
            let events: Vec<Value> = c.get(&format!("/v1/runs/{}/events", run.run_id))?;
            for e in events {
                println!("{e}");
            }
        }
        Cmd::Verify => {
            let p = Project::require(&cwd)?;
            let run = ops::require_run(&c, &p)?;
            let r: ChainReport = c.get(&format!("/v1/runs/{}/verify", run.run_id))?;
            if !r.ok {
                bail!(
                    "chain broken after {} events: {}",
                    r.events,
                    r.error.unwrap_or_default()
                );
            }
            println!(
                "ok: {} events, {} signed, {} unsigned (v1), head {}",
                r.events,
                r.signed,
                r.unsigned,
                r.head_hash.unwrap_or_default()
            );
        }
        Cmd::Export { tier } => {
            let p = Project::require(&cwd)?;
            let run = ops::require_run(&c, &p)?;
            let export: speedrun_core::api::ExportResponse =
                c.get(&format!("/v1/runs/{}/export?tier={tier}", run.run_id))?;
            println!("{}", serde_json::to_string_pretty(&export)?);
        }
        Cmd::Categories => {
            let r: CategoriesResponse = c.get("/v1/categories")?;
            for k in r.categories {
                println!("{:<16} v{}  {} — {}", k.id, k.version, k.name, k.description);
            }
        }
        Cmd::Flush => {
            c.get::<Value>("/v1/health")?; // Starts the daemon, which also flushes.
            println!("delivered {} spooled events", c.flush_spool());
        }
        Cmd::Ingest {
            kind,
            host,
            session,
            model,
        } => {
            // Adapters parse stdout; never fail them.
            let out = ingest(&c, &cwd, &kind, &host, &session, model.as_deref())
                .unwrap_or_else(|e| serde_json::json!({ "recorded": false, "error": format!("{e:#}") }));
            println!("{out}");
        }
        Cmd::Tool { list: true, .. } => println!("{}", serde_json::to_string_pretty(&tools::definitions())?),
        Cmd::Tool { name, args, .. } => {
            let args: Value =
                serde_json::from_str(&args).map_err(|e| anyhow::anyhow!("--args is not JSON: {e}"))?;
            match tools::call(name.as_deref().unwrap_or_default(), &args, &cwd) {
                Ok(text) => println!("{text}"),
                Err(e) => {
                    println!("{e:#}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Watch { since_hours, once } => watch::watch(since_hours, once)?,
        Cmd::Display { json } => {
            c.ready()?;
            let token = std::fs::read_to_string(speedrun_core::paths::display_token_path())?;
            let base = c.base_url();
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "base_url": base, "token": token.trim() })
                );
                return Ok(());
            }
            println!("display token (read-only, no prompt text): {}", token.trim());
            println!("snapshot: GET {base}/v1/live");
            println!("stream:   GET {base}/v1/live/stream   (server-sent events, event `snapshot`)");
            println!("auth:     Authorization: Bearer <token>, or ?token=<token> for EventSource");
            println!("browser pages need SPEEDRUN_CORS_ORIGINS=<origin> set for speedrund");
        }
        Cmd::Setup { host, project, force } => {
            let root = if project {
                Some(Project::find(&cwd)?.map_or_else(|| cwd.clone(), |p| p.root))
            } else {
                None
            };
            let var = |k: &str| std::env::var_os(k).map(std::path::PathBuf::from);
            let home = var("HOME").context("HOME is not set")?;
            let (xdg, copilot) = (var("XDG_CONFIG_HOME"), var("COPILOT_HOME"));
            let dirs = setup::Dirs {
                home: &home,
                xdg_config: xdg.as_deref(),
                copilot_home: copilot.as_deref(),
            };
            let base = setup::target_dir(host, root.as_deref(), &dirs);
            for outcome in setup::install(host, &base, force)? {
                match outcome {
                    setup::Outcome::Installed(p) => println!("installed {}", p.display()),
                    setup::Outcome::Upgraded(p) => println!("updated   {}", p.display()),
                    setup::Outcome::Unchanged(p) => println!("current   {}", p.display()),
                }
            }
            if !setup::on_path() {
                println!(
                    "warning: `speedrun` is not on PATH; the adapter will not find it. Add it, or set SPEEDRUN_BIN."
                );
            }
            println!("Restart the host to load it. Then run `speedrun init` in each repo you want timed.");
        }
        Cmd::Secret { action } => match action {
            SecretCmd::Set { name } => secret::set(name)?,
            SecretCmd::Rm { name } => secret::remove(name)?,
            SecretCmd::List => secret::list(),
        },
        Cmd::Key { action } => match action {
            KeyCmd::Rotate => {
                let r: RotateResponse = c.post("/v1/device/rotate", &serde_json::json!({}))?;
                println!("rotated: {} replaces {}", r.device_pubkey, r.previous_pubkey);
            }
        },
        Cmd::Mcp { project_dir } => mcp::serve(project_dir.as_deref())?,
        Cmd::Schema => println!(
            "{}",
            serde_json::to_string_pretty(&speedrun_core::api::protocol_schema())?
        ),
        Cmd::Hook { .. } => unreachable!("handled in main"),
    }
    Ok(())
}

fn ingest(
    c: &Client,
    cwd: &std::path::Path,
    kind: &str,
    host: &str,
    session: &str,
    model: Option<&str>,
) -> Result<Value> {
    let Some(p) = Project::find(cwd)? else {
        return Ok(serde_json::json!({ "recorded": false, "reason": "not a speedrun project" }));
    };
    if kind == "turn-end" {
        let r = ops::append(
            c,
            &p,
            EventPayload::AgentTurnEnded {
                session_id: session.to_string(),
            },
        )?;
        return Ok(serde_json::json!({ "recorded": true, "spooled": r.is_none() }));
    }
    let mut content = String::new();
    std::io::stdin().read_to_string(&mut content)?;
    Ok(match ops::record_prompt(c, &p, host, session, model, &content)? {
        Some(r) => serde_json::json!({
            "recorded": true,
            "spooled": false,
            "run_started": r.run_started,
            "run_id": r.run_id,
            "context": r.run_started.then(|| ops::run_started_context(&p, &r.run_id)),
        }),
        None => serde_json::json!({ "recorded": true, "spooled": true, "run_started": false }),
    })
}

fn milestone_event(
    c: &Client,
    cwd: &std::path::Path,
    milestone: &str,
    make: impl FnOnce(String) -> EventPayload,
) -> Result<()> {
    let p = Project::require(cwd)?;
    let run = ops::require_run(c, &p)?;
    ops::append_now(c, &p, make(ops::milestone_id(&run, milestone)?))?;
    print!("{}", ops::format_status(&ops::require_run(c, &p)?));
    Ok(())
}
