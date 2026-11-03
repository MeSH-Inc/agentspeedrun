//! Read-only capture readiness shared by the CLI and editor adapters.
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use speedrun_core::api::CaptureSummary;

use crate::{
    client::{self, Client},
    project::Project,
    setup::{self, Dirs, Host},
};

#[derive(Serialize)]
pub struct Readiness {
    pub version: u8,
    pub project: &'static str,
    pub daemon: &'static str,
    pub integrations: Vec<&'static str>,
    pub selected_integration: Option<(String, String)>,
    pub integration_state: &'static str,
    pub recording: &'static str,
    pub last_activity: Option<String>,
    pub last_host: Option<String>,
    pub queue_scope: &'static str,
    pub queued: u64,
    pub rejected: u64,
    pub write_failures: u64,
    pub next_action: &'static str,
    pub next_action_id: &'static str,
}

pub fn inspect(client: &Client, cwd: &Path) -> Result<Readiness> {
    let project = Project::find(cwd)?;
    let project_state = match &project {
        None => "untracked",
        Some(p) if !p.is_trusted()? => "untrusted",
        Some(_) => "trusted",
    };
    let integrations = installed(project.as_ref().map(|p| p.root.as_path()));
    let selected_integration = project
        .as_ref()
        .map(|p| crate::onboarding::selected_integration(&p.config.project_id))
        .transpose()?
        .flatten();
    let integration_state = match selected_integration.as_ref() {
        Some((_, scope)) if scope == "external" => "external",
        Some((host, _)) => {
            let adapter = if host == "vscode" {
                "copilot"
            } else {
                host.as_str()
            };
            if integrations.contains(&adapter) {
                "installed"
            } else {
                "missing"
            }
        }
        None if integrations.is_empty() => "missing",
        None => "installed",
    };
    let stats = client::spool_stats()?;
    let daemon = match client.verify_health() {
        Ok(_) => "online",
        Err(client::CallError::WrongDataDir { .. }) => "wrong_data_dir",
        Err(_) => "offline",
    };
    let mut recording = "unknown";
    let mut last_activity = None;
    let mut last_host = None;
    if daemon == "online" && project_state == "trusted" {
        let p = project.as_ref().expect("trusted project exists");
        let response: Result<CaptureSummary, _> =
            client.get_existing(&format!("/v1/projects/{}/capture-summary", p.config.project_id));
        if let Ok(summary) = response {
            recording = if summary.last_prompt_at.is_some() {
                "captured"
            } else {
                "awaiting_prompt"
            };
            last_activity = summary.last_prompt_at.map(|at| at.to_rfc3339());
            last_host = summary.last_host;
        }
    }
    let (next_action_id, next_action) = if project_state == "untracked" {
        ("setup", "Run speedrun setup in this project.")
    } else if project_state == "untrusted" {
        ("trust_project", "Run speedrun init --trust in this project.")
    } else if daemon == "wrong_data_dir" {
        (
            "doctor",
            "Check the daemon address and data directory with speedrun doctor.",
        )
    } else if daemon == "offline" {
        (
            "flush",
            "Run speedrun flush to start the daemon and deliver queued requests.",
        )
    } else if stats.rejected > 0 {
        (
            "inspect_rejected",
            "Inspect rejected requests with speedrun outbox rejected.",
        )
    } else if stats.write_failures > 0 {
        ("doctor", "Inspect spool write failures with speedrun doctor.")
    } else if stats.spooled > 0 {
        ("flush", "Run speedrun flush to deliver queued requests.")
    } else if recording == "unknown" {
        ("doctor", "Check speedrun doctor and retry readiness.")
    } else if integration_state == "missing" && selected_integration.is_some() && recording == "captured" {
        (
            "setup",
            "The selected integration is missing; run speedrun setup to reinstall it. Past capture remains recorded.",
        )
    } else if recording == "captured" {
        ("none", "Capture has been observed in this current run.")
    } else if integration_state == "missing" {
        ("setup", "Install an integration with speedrun setup.")
    } else {
        (
            "send_prompt",
            "Send an agent prompt in this project to verify capture.",
        )
    };
    Ok(Readiness {
        version: 1,
        project: project_state,
        daemon,
        integrations,
        selected_integration,
        integration_state,
        recording,
        last_activity,
        last_host,
        queue_scope: "device",
        queued: stats.spooled,
        rejected: stats.rejected,
        write_failures: stats.write_failures,
        next_action,
        next_action_id,
    })
}

fn installed(project: Option<&Path>) -> Vec<&'static str> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let copilot = std::env::var_os("COPILOT_HOME").map(PathBuf::from);
    let dirs = Dirs {
        home: &home,
        xdg_config: xdg.as_deref(),
        copilot_home: copilot.as_deref(),
    };
    [
        (
            Host::Opencode,
            "opencode",
            "plugins/agentspeedrun.ts",
            "mesh-inc/agentspeedrun",
        ),
        (
            Host::Pi,
            "pi",
            "extensions/agentspeedrun.ts",
            "mesh-inc/agentspeedrun",
        ),
        (
            Host::Omp,
            "omp",
            "extensions/agentspeedrun.ts",
            "mesh-inc/agentspeedrun",
        ),
        (
            Host::Copilot,
            "copilot",
            "hooks/agentspeedrun.json",
            "speedrun hook copilot",
        ),
    ]
    .into_iter()
    .filter_map(|(host, name, file, marker)| {
        [Some(project), Some(None)]
            .into_iter()
            .flatten()
            .any(|root| {
                std::fs::read_to_string(setup::target_dir(host, root, &dirs).join(file))
                    .is_ok_and(|text| text.contains(marker))
            })
            .then_some(name)
    })
    .collect()
}

pub fn print(state: &Readiness) {
    println!(
        "Project: {}\nDaemon: {}\nInstalled integrations: {}\nSelected integration: {}\nIntegration state: {}\nRecording: {}\nLast activity: {} by {}\nDevice queue: {} · rejected: {} · write failures: {}\nNext: {}",
        state.project,
        state.daemon,
        if state.integrations.is_empty() {
            "none detected".into()
        } else {
            state.integrations.join(", ")
        },
        state
            .selected_integration
            .as_ref()
            .map_or_else(|| "none".into(), |(host, scope)| format!("{host} ({scope})")),
        state.integration_state,
        state.recording,
        state.last_activity.as_deref().unwrap_or("none observed"),
        state.last_host.as_deref().unwrap_or("unknown host"),
        state.queued,
        state.rejected,
        state.write_failures,
        state.next_action
    );
}
