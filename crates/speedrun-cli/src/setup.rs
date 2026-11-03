//! `speedrun setup <host>`: install the host adapter shipped inside this
//! binary, so adapter and CLI versions always match.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const OPENCODE_PLUGIN: &str = include_str!("../../../plugins/opencode/agentspeedrun.ts");
const PI_EXTENSION: &str = include_str!("../../../plugins/pi/agentspeedrun.ts");
const COPILOT_HOOKS: &str = include_str!("../../../plugins/copilot/agentspeedrun.json");
/// Present in every adapter we ship; files without it are not ours to overwrite.
const MARKER: &str = "mesh-inc/agentspeedrun";
/// JSON has no comments, so the hooks file is recognized by its commands.
const COPILOT_MARKER: &str = "speedrun hook copilot";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Host {
    Opencode,
    Pi,
    /// oh-my-pi
    Omp,
    /// GitHub Copilot agent hooks, read by VS Code's agent and the Copilot CLI.
    Copilot,
}

/// The parts of the environment that decide where hosts look.
pub struct Dirs<'a> {
    pub home: &'a Path,
    pub xdg_config: Option<&'a Path>,
    pub copilot_home: Option<&'a Path>,
}

/// Where a host loads adapters from, user-wide or for one project.
pub fn target_dir(host: Host, project_root: Option<&Path>, dirs: &Dirs<'_>) -> PathBuf {
    let home = dirs.home;
    match (host, project_root) {
        (Host::Opencode, Some(p)) => p.join(".opencode"),
        (Host::Opencode, None) => dirs
            .xdg_config
            .map_or_else(|| home.join(".config"), Path::to_path_buf)
            .join("opencode"),
        (Host::Pi, Some(p)) => p.join(".pi"),
        (Host::Pi, None) => home.join(".pi/agent"),
        (Host::Omp, Some(p)) => p.join(".omp"),
        (Host::Omp, None) => home.join(".omp/agent"),
        (Host::Copilot, Some(p)) => p.join(".github"),
        (Host::Copilot, None) => dirs
            .copilot_home
            .map_or_else(|| home.join(".copilot"), Path::to_path_buf),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Installed(PathBuf),
    Upgraded(PathBuf),
    Unchanged(PathBuf),
}

/// Check deterministic conflicts before making any setup changes.
pub fn validate(host: Host, base: &Path, force: bool) -> Result<()> {
    let (subdir, name, marker) = match host {
        Host::Opencode => ("plugins", "agentspeedrun.ts", MARKER),
        Host::Pi | Host::Omp => ("extensions", "agentspeedrun.ts", MARKER),
        Host::Copilot => ("hooks", "agentspeedrun.json", COPILOT_MARKER),
    };
    let file = base.join(subdir).join(name);
    match std::fs::read_to_string(&file) {
        Ok(existing) if !existing.contains(marker) && !force => bail!(
            "{} exists and was not written by speedrun; use --force to replace it",
            file.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
    }
    Ok(())
}

/// Install the adapter for `host` under its config dir `base`.
pub fn install(host: Host, base: &Path, force: bool) -> Result<Vec<Outcome>> {
    validate(host, base, force)?;
    let (subdir, file_name, source, marker) = match host {
        Host::Opencode => ("plugins", "agentspeedrun.ts", OPENCODE_PLUGIN, MARKER),
        Host::Pi | Host::Omp => ("extensions", "agentspeedrun.ts", PI_EXTENSION, MARKER),
        Host::Copilot => ("hooks", "agentspeedrun.json", COPILOT_HOOKS, COPILOT_MARKER),
    };
    let file = base.join(subdir).join(file_name);
    let outcome = match std::fs::read_to_string(&file) {
        Ok(existing) if existing == source => Outcome::Unchanged(file.clone()),
        Ok(existing) if !existing.contains(marker) && !force => {
            bail!(
                "{} exists and was not written by speedrun; use --force to replace it",
                file.display()
            )
        }
        Ok(_) => Outcome::Upgraded(file.clone()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Outcome::Installed(file.clone()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
    };
    if !matches!(outcome, Outcome::Unchanged(_)) {
        std::fs::create_dir_all(file.parent().expect("has parent"))?;
        std::fs::write(&file, source).with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(vec![outcome])
}

/// Remove only an adapter bearing our marker. User configuration and dependencies remain.
pub fn remove(host: Host, base: &Path) -> Result<Option<PathBuf>> {
    let (subdir, name, marker) = match host {
        Host::Opencode => ("plugins", "agentspeedrun.ts", MARKER),
        Host::Pi | Host::Omp => ("extensions", "agentspeedrun.ts", MARKER),
        Host::Copilot => ("hooks", "agentspeedrun.json", COPILOT_MARKER),
    };
    let file = base.join(subdir).join(name);
    match std::fs::read_to_string(&file) {
        Ok(existing) if existing.contains(marker) => {
            std::fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
            Ok(Some(file))
        }
        Ok(_) => bail!(
            "{} was not written by speedrun; leaving it untouched",
            file.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", file.display())),
    }
}

/// Is `speedrun` resolvable on PATH, where the adapters will look for it?
pub fn on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("speedrun").is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn installs_upgrades_and_leaves_foreign_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("pi");
        let file = base.join("extensions").join("agentspeedrun.ts");
        assert_eq!(
            install(Host::Pi, &base, false).unwrap(),
            vec![Outcome::Installed(file.clone())]
        );
        assert_eq!(
            install(Host::Pi, &base, false).unwrap(),
            vec![Outcome::Unchanged(file.clone())]
        );

        std::fs::write(&file, format!("// old {MARKER} version")).unwrap();
        assert_eq!(
            install(Host::Pi, &base, false).unwrap(),
            vec![Outcome::Upgraded(file.clone())]
        );

        std::fs::write(&file, "// someone else's extension").unwrap();
        assert!(install(Host::Pi, &base, false).is_err());
        assert!(install(Host::Pi, &base, true).is_ok());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), PI_EXTENSION);
    }

    #[test]
    fn removal_preserves_foreign_files_and_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plugins/agentspeedrun.ts");
        std::fs::write(dir.path().join("package.json"), "user configuration").unwrap();
        install(Host::Opencode, dir.path(), false).unwrap();
        assert_eq!(remove(Host::Opencode, dir.path()).unwrap(), Some(file.clone()));
        assert!(dir.path().join("package.json").exists());
        assert_eq!(remove(Host::Opencode, dir.path()).unwrap(), None);
        std::fs::write(&file, "foreign plugin").unwrap();
        assert!(remove(Host::Opencode, dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "foreign plugin");
    }

    #[test]
    fn opencode_install_does_not_modify_package_json() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("package.json");
        std::fs::write(&pkg, "user configuration").unwrap();
        install(Host::Opencode, dir.path(), false).unwrap();
        assert_eq!(std::fs::read_to_string(pkg).unwrap(), "user configuration");
    }

    #[test]
    fn target_dirs_match_host_conventions() {
        let plain = Dirs {
            home: Path::new("/h"),
            xdg_config: None,
            copilot_home: None,
        };
        let custom = Dirs {
            home: Path::new("/h"),
            xdg_config: Some(Path::new("/x")),
            copilot_home: Some(Path::new("/c")),
        };
        let proj = Some(Path::new("/p"));
        assert_eq!(
            target_dir(Host::Opencode, None, &plain),
            Path::new("/h/.config/opencode")
        );
        assert_eq!(
            target_dir(Host::Opencode, None, &custom),
            Path::new("/x/opencode")
        );
        assert_eq!(
            target_dir(Host::Opencode, proj, &plain),
            Path::new("/p/.opencode")
        );
        assert_eq!(target_dir(Host::Pi, None, &plain), Path::new("/h/.pi/agent"));
        assert_eq!(target_dir(Host::Omp, proj, &plain), Path::new("/p/.omp"));
        assert_eq!(target_dir(Host::Copilot, proj, &plain), Path::new("/p/.github"));
        assert_eq!(target_dir(Host::Copilot, None, &plain), Path::new("/h/.copilot"));
        assert_eq!(target_dir(Host::Copilot, None, &custom), Path::new("/c"));
    }

    #[test]
    fn copilot_hooks_install_as_json_and_respect_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks/agentspeedrun.json");
        assert_eq!(
            install(Host::Copilot, dir.path(), false).unwrap(),
            vec![Outcome::Installed(file.clone())]
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        for event in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
            assert!(
                v["hooks"][event][0]["command"]
                    .as_str()
                    .unwrap()
                    .starts_with(COPILOT_MARKER)
            );
        }
        std::fs::write(&file, r#"{"version":1,"hooks":{}}"#).unwrap();
        assert!(install(Host::Copilot, dir.path(), false).is_err());
    }

    #[test]
    fn shipped_adapters_carry_the_marker() {
        assert!(OPENCODE_PLUGIN.contains(MARKER) && PI_EXTENSION.contains(MARKER));
        assert!(COPILOT_HOOKS.contains(COPILOT_MARKER));
    }
}
