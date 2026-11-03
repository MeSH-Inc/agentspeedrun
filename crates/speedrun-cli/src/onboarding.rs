//! Interactive onboarding; the explicit `setup HOST` command remains adapter-only.
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use speedrun_core::category::{self, DEFAULT_CATEGORY};

use crate::{preferences, project::Project, setup};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Host {
    Opencode,
    Pi,
    Omp,
    Copilot,
    Claude,
    Vscode,
    Zed,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Scope {
    Project,
    User,
}

fn selection_path(project_id: &str) -> PathBuf {
    let name: String = project_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    speedrun_core::paths::data_dir()
        .join("integrations")
        .join(format!("{name}.json"))
}

/// Read the last guided setup selection for this project, without changing state.
pub(crate) fn selected_integration(project_id: &str) -> Result<Option<(String, String)>> {
    let path = selection_path(project_id);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("reading integration selection")?,
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn save_selection(project_id: &str, host: Host, scope: Option<Scope>) -> Result<()> {
    let path = selection_path(project_id);
    std::fs::create_dir_all(path.parent().expect("selection has parent"))?;
    let host = host
        .to_possible_value()
        .expect("host value")
        .get_name()
        .to_owned();
    let scope = scope.map_or_else(
        || "external".to_owned(),
        |scope| {
            scope
                .to_possible_value()
                .expect("scope value")
                .get_name()
                .to_owned()
        },
    );
    let temp = path.with_extension(format!("{}.tmp", speedrun_core::new_id()));
    std::fs::write(&temp, serde_json::to_vec(&(host, scope))?)?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

pub struct Options {
    pub host: Option<Host>,
    pub scope: Option<Scope>,
    pub category: Option<String>,
    pub capture: Option<preferences::CaptureArgument>,
    pub yes: bool,
    pub force: bool,
}

fn answer(prompt: &str, default: Option<&str>) -> Result<String> {
    loop {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            bail!("setup cancelled: input ended");
        }
        let value = line.trim();
        if !value.is_empty() {
            return Ok(value.to_owned());
        }
        if let Some(default) = default {
            return Ok(default.to_owned());
        }
    }
}

fn choose<T: ValueEnum + Clone>(prompt: &str, default: Option<&str>) -> Result<T> {
    loop {
        let value = answer(prompt, default)?;
        if let Ok(choice) = T::from_str(&value, true) {
            return Ok(choice);
        }
        println!("Invalid choice: {value}");
    }
}

fn detected(host: &str, config: &Path) -> String {
    let binary = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            std::fs::metadata(dir.join(host))
                .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        })
    });
    match (binary, config.exists()) {
        (true, true) => "binary on PATH, config present",
        (true, false) => "binary on PATH",
        (false, true) => "config present",
        (false, false) => "not detected",
    }
    .into()
}

pub fn run(cwd: &Path, options: Options) -> Result<()> {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    if !interactive && !options.yes {
        bail!(
            "guided setup requires a terminal; for noninteractive use provide --yes, --host, --capture, and --scope when needed"
        );
    }
    let var = |key: &str| std::env::var_os(key).map(PathBuf::from);
    let home = var("HOME").context("HOME is not set")?;
    let (xdg, copilot) = (var("XDG_CONFIG_HOME"), var("COPILOT_HOME"));
    let dirs = setup::Dirs {
        home: &home,
        xdg_config: xdg.as_deref(),
        copilot_home: copilot.as_deref(),
    };
    let existing = Project::find(cwd)?;
    println!("Host detection (presence does not prove an adapter is installed):");
    for (name, binary, path) in [
        (
            "opencode",
            "opencode",
            setup::target_dir(setup::Host::Opencode, None, &dirs),
        ),
        ("pi", "pi", setup::target_dir(setup::Host::Pi, None, &dirs)),
        ("omp", "omp", setup::target_dir(setup::Host::Omp, None, &dirs)),
        (
            "copilot",
            "copilot",
            setup::target_dir(setup::Host::Copilot, None, &dirs),
        ),
        ("claude", "claude", home.join(".claude")),
        ("vscode", "code", home.join(".vscode")),
        ("zed", "zed", home.join(".config/zed")),
    ] {
        println!("  {name}: {}", detected(binary, &path));
    }
    let host = match options.host {
        Some(host) => host,
        None if interactive => choose("Host (opencode/pi/omp/copilot/claude/vscode/zed): ", None)?,
        None => bail!("noninteractive setup requires --host"),
    };
    let adapter = match host {
        Host::Opencode => Some(setup::Host::Opencode),
        Host::Pi => Some(setup::Host::Pi),
        Host::Omp => Some(setup::Host::Omp),
        Host::Copilot | Host::Vscode => Some(setup::Host::Copilot),
        Host::Claude | Host::Zed => None,
    };
    let scope = if adapter.is_some() {
        Some(match options.scope {
            Some(scope) => scope,
            None if interactive => choose("Adapter scope (project/user) [project]: ", Some("project"))?,
            None => bail!("noninteractive setup requires --scope project or --scope user"),
        })
    } else {
        if options.scope.is_some() {
            bail!("--scope applies only to embedded adapters");
        }
        None
    };
    let category = if let Some(project) = &existing {
        if options
            .category
            .as_deref()
            .is_some_and(|category| category != project.config.category_id)
        {
            bail!(
                "this project already uses category {}; setup will not replace it",
                project.config.category_id
            );
        }
        println!(
            "Existing project: {} (category {})",
            project.root.display(),
            project.config.category_id
        );
        None
    } else {
        let category = match options.category {
            Some(category) => category,
            None if interactive => answer(
                "Category (any-percent/hundred-percent/first-revenue) [any-percent]: ",
                Some(DEFAULT_CATEGORY),
            )?,
            None => DEFAULT_CATEGORY.to_owned(),
        };
        if category::find(&category::builtin()?, &category, None).is_none() {
            bail!("unknown category {category}");
        }
        Some(category)
    };
    println!(
        "Prompt capture is device-wide: content stores redacted prompt text; metadata-only stores no prompt text."
    );
    let capture = match options.capture {
        Some(capture) => capture,
        None if interactive => choose("Capture (content/metadata-only): ", None)?,
        None => bail!("noninteractive setup requires --capture content or --capture metadata-only"),
    };
    if interactive && !options.yes {
        let action = if existing.is_some() {
            "trust this existing project"
        } else {
            "initialize and trust this project"
        };
        let confirm = answer(
            &format!("Save capture preference and {action}? [y/N]: "),
            Some("n"),
        )?;
        if !matches!(confirm.to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("setup cancelled");
        }
    }
    if let Some(adapter) = adapter {
        let root = existing
            .as_ref()
            .map_or_else(|| Project::init_root(cwd), |project| project.root.clone());
        let target = match scope.expect("adapter scope") {
            Scope::Project => Some(root.as_path()),
            Scope::User => None,
        };
        setup::validate(adapter, &setup::target_dir(adapter, target, &dirs), options.force)?;
    }
    // Save the deliberate capture choice before enabling any project or hooks.
    preferences::set(capture.into())?;
    let project = match existing {
        Some(project) => {
            project.trust()?;
            println!("Trusted {}", project.root.display());
            project
        }
        None => {
            let project = Project::init(cwd, category.as_deref().expect("new project category"), false)?;
            println!(
                "Initialized and trusted {}. Commit .speedrun/project.json.",
                project.root.display()
            );
            project
        }
    };
    if let Some(adapter) = adapter {
        let root = match scope.expect("adapter scope") {
            Scope::Project => Some(project.root.as_path()),
            Scope::User => None,
        };
        let base = setup::target_dir(adapter, root, &dirs);
        for outcome in setup::install(adapter, &base, options.force)? {
            match outcome {
                setup::Outcome::Installed(p) => println!("installed {}", p.display()),
                setup::Outcome::Upgraded(p) => println!("updated   {}", p.display()),
                setup::Outcome::Unchanged(p) => println!("current   {}", p.display()),
            }
        }
    }
    save_selection(&project.config.project_id, host, scope)?;
    if let Some((selected_host, selected_scope)) = selected_integration(&project.config.project_id)? {
        println!("Integration selection: {selected_host} ({selected_scope})");
    }
    match host {
        Host::Claude => println!(
            "In Claude Code, run /plugin marketplace add mesh-inc/agentspeedrun, then /plugin install agentspeedrun@agentspeedrun."
        ),
        Host::Vscode => println!(
            "Copilot hooks capture prompts. The optional VS Code extension adds a timer, milestone view and MCP tools; install its VSIX from the release."
        ),
        Host::Zed => println!(
            "Zed's extension provides MCP tools but cannot capture its own agent prompts. Install it as a dev extension from plugins/zed, and use a supported capture host to start runs."
        ),
        _ => {}
    }
    if !setup::on_path() {
        println!(
            "warning: `speedrun` is not on PATH; adapters may not find it. Add it or configure the host's binary path."
        );
    }
    println!(
        "Setup configured; prompt capture has not been verified. Restart the capture host, send a real agent prompt in {}, then run `speedrun capture` there to check it.",
        project.root.display()
    );
    Ok(())
}
