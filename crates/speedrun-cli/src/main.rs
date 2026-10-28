//! `speedrun`: CLI, host hook adapter and MCP server for agentspeedrun.

mod client;
mod hook;
mod mcp;
mod ops;
mod project;
mod watch;

use std::io::Read;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;
use speedrun_core::{
    EventPayload, Evidence, MilestoneOrigin, RunOutcome, Trust,
    api::{CategoriesResponse, ChainReport},
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
        #[arg(long)]
        json: bool,
    },
    /// Mark a milestone as reached. Attach a commit to have it verified.
    Split {
        /// Milestone id or category key.
        milestone: String,
        /// Commit that achieves it (SHA or ref such as HEAD).
        #[arg(long)]
        commit: Option<String>,
        #[arg(long)]
        url: Vec<String>,
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
    /// Reads the prompt text from stdin for `prompt`.
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
    Display,
    /// Run the MCP server on stdio.
    Mcp,
    /// Print the JSON Schema of the plugin ↔ daemon protocol.
    Schema,
}

fn main() {
    let cli = Cli::parse();
    // Hooks must never fail the host.
    if let Cmd::Hook { host, event } = &cli.command {
        match host.as_str() {
            "claude-code" => hook::claude_code(event),
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
        Cmd::Status { json } => {
            let p = Project::require(&cwd)?;
            match ops::active_run(&c, &p)? {
                Some(run) if json => println!("{}", serde_json::to_string_pretty(&run)?),
                Some(run) => print!("{}", ops::format_status(&run)),
                None => println!("No active run; the timer starts at the next prompt."),
            }
        }
        Cmd::Split {
            milestone,
            commit,
            url,
            note,
        } => {
            let p = Project::require(&cwd)?;
            let run = ops::require_run(&c, &p)?;
            let milestone_id = ops::milestone_id(&run, &milestone)?;
            let mut evidence = Vec::new();
            if let Some(rev) = commit {
                evidence.push(Evidence::GitCommit {
                    sha: p.resolve_commit(&rev)?,
                });
            }
            evidence.extend(url.into_iter().map(|url| Evidence::Url { url }));
            evidence.extend(note.map(|text| Evidence::Note { text }));
            ops::append_now(
                &c,
                &p,
                EventPayload::MilestoneReached {
                    milestone_id,
                    trust: Trust::SelfDeclared,
                    evidence,
                },
            )?;
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
                },
            )?;
            println!("Added {milestone_id}");
        }
        Cmd::Accept { milestone } => milestone_event(&c, &cwd, &milestone, |milestone_id| {
            EventPayload::MilestoneAccepted { milestone_id }
        })?,
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
                "ok: {} events, head {}",
                r.events,
                r.head_hash.unwrap_or_default()
            );
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
            let Some(p) = Project::find(&cwd)? else {
                return Ok(());
            };
            if kind == "prompt" {
                let mut content = String::new();
                std::io::stdin().read_to_string(&mut content)?;
                ops::record_prompt(&c, &p, &host, &session, model.as_deref(), &content)?;
            } else {
                ops::append(&c, &p, EventPayload::AgentTurnEnded { session_id: session })?;
            }
        }
        Cmd::Watch { since_hours, once } => watch::watch(since_hours, once)?,
        Cmd::Display => {
            c.ready()?;
            let token = std::fs::read_to_string(speedrun_core::paths::display_token_path())?;
            let base = c.base_url();
            println!("display token (read-only, no prompt text): {}", token.trim());
            println!("snapshot: GET {base}/v1/live");
            println!("stream:   GET {base}/v1/live/stream   (server-sent events, event `snapshot`)");
            println!("auth:     Authorization: Bearer <token>, or ?token=<token> for EventSource");
            println!("browser pages need SPEEDRUN_CORS_ORIGINS=<origin> set for speedrund");
        }
        Cmd::Mcp => mcp::serve()?,
        Cmd::Schema => println!(
            "{}",
            serde_json::to_string_pretty(&speedrun_core::api::protocol_schema())?
        ),
        Cmd::Hook { .. } => unreachable!("handled in main"),
    }
    Ok(())
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
