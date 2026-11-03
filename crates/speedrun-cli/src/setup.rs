//! `speedrun setup <host>`: install the host adapter shipped inside this
//! binary, so adapter and CLI versions always match.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

const OPENCODE_PLUGIN: &str = include_str!("../../../plugins/opencode/agentspeedrun.ts");
const PI_EXTENSION: &str = include_str!("../../../plugins/pi/agentspeedrun.ts");
const COPILOT_HOOKS: &str = include_str!("../../../plugins/copilot/agentspeedrun.json");
/// Present in every adapter we ship; files without it are not ours to overwrite.
const MARKER: &str = "mesh-inc/agentspeedrun";
/// JSON has no comments, so the hooks file is recognized by its commands.
const COPILOT_MARKER: &str = "speedrun hook copilot";
const OPENCODE_PLUGIN_DEP: (&str, &str) = ("@opencode-ai/plugin", "^1.19.5");

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
    if host == Host::Opencode {
        let path = base.join("package.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let value: Value =
                    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                let object = value
                    .as_object()
                    .with_context(|| format!("{} is not a JSON object", path.display()))?;
                if object.get("dependencies").is_some_and(|deps| !deps.is_object()) {
                    bail!("{}: dependencies is not an object", path.display());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        }
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
    let mut out = vec![outcome];
    if host == Host::Opencode {
        // opencode runs `bun install` in its config dir at startup; the plugin imports this package.
        out.extend(ensure_dependency(
            &base.join("package.json"),
            OPENCODE_PLUGIN_DEP,
        )?);
    }
    Ok(out)
}

/// Add `dep` to package.json's dependencies unless already present. Keeps
/// every other key and their order.
fn ensure_dependency(path: &Path, (name, version): (&str, &str)) -> Result<Option<Outcome>> {
    let mut pkg: Value = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let Some(obj) = pkg.as_object_mut() else {
        bail!("{} is not a JSON object", path.display());
    };
    let deps = obj
        .entry("dependencies")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(deps) = deps.as_object_mut() else {
        bail!("{}: dependencies is not an object", path.display());
    };
    if deps.contains_key(name) {
        return Ok(None);
    }
    let existed = path.exists();
    deps.insert(name.to_string(), Value::String(version.to_string()));
    std::fs::write(path, serde_json::to_string_pretty(&pkg)? + "\n")?;
    Ok(Some(if existed {
        Outcome::Upgraded(path.to_path_buf())
    } else {
        Outcome::Installed(path.to_path_buf())
    }))
}

/// Is `speedrun` resolvable on PATH, where the adapters will look for it?
pub fn on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("speedrun").is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn opencode_adds_plugin_dependency_without_touching_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("package.json");
        std::fs::write(
            &pkg,
            r#"{"name":"cfg","dependencies":{"zeta":"1","alpha":"2"},"private":true}"#,
        )
        .unwrap();
        install(Host::Opencode, dir.path(), false).unwrap();
        let text = std::fs::read_to_string(&pkg).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["dependencies"]["@opencode-ai/plugin"], "^1.19.5");
        assert_eq!(v["dependencies"]["zeta"], "1");
        assert!(
            text.find("zeta").unwrap() < text.find("alpha").unwrap(),
            "key order preserved"
        );
        assert!(text.find("\"name\"").unwrap() < text.find("\"private\"").unwrap());

        // A user's own pin wins.
        std::fs::write(&pkg, r#"{"dependencies":{"@opencode-ai/plugin":"1.20.0"}}"#).unwrap();
        install(Host::Opencode, dir.path(), false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&pkg).unwrap()).unwrap();
        assert_eq!(v["dependencies"]["@opencode-ai/plugin"], "1.20.0");
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
