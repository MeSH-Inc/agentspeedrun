//! `speedrun setup <host>`: install the host adapter shipped inside this
//! binary, so adapter and CLI versions always match.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

const OPENCODE_PLUGIN: &str = include_str!("../../../plugins/opencode/agentspeedrun.ts");
const PI_EXTENSION: &str = include_str!("../../../plugins/pi/agentspeedrun.ts");
/// Present in every adapter we ship; files without it are not ours to overwrite.
const MARKER: &str = "mesh-inc/agentspeedrun";
const FILE_NAME: &str = "agentspeedrun.ts";
const OPENCODE_PLUGIN_DEP: (&str, &str) = ("@opencode-ai/plugin", "^1.19.5");

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Host {
    Opencode,
    Pi,
    /// oh-my-pi
    Omp,
}

/// Where a host loads adapters from, user-wide or for one project.
pub fn target_dir(
    host: Host,
    project_root: Option<&Path>,
    home: &Path,
    xdg_config: Option<&Path>,
) -> PathBuf {
    match (host, project_root) {
        (Host::Opencode, Some(p)) => p.join(".opencode"),
        (Host::Opencode, None) => xdg_config
            .map_or_else(|| home.join(".config"), Path::to_path_buf)
            .join("opencode"),
        (Host::Pi, Some(p)) => p.join(".pi"),
        (Host::Pi, None) => home.join(".pi/agent"),
        (Host::Omp, Some(p)) => p.join(".omp"),
        (Host::Omp, None) => home.join(".omp/agent"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Installed(PathBuf),
    Upgraded(PathBuf),
    Unchanged(PathBuf),
}

/// Install the adapter for `host` under its config dir `base`.
pub fn install(host: Host, base: &Path, force: bool) -> Result<Vec<Outcome>> {
    let (subdir, source) = match host {
        Host::Opencode => ("plugins", OPENCODE_PLUGIN),
        Host::Pi | Host::Omp => ("extensions", PI_EXTENSION),
    };
    let file = base.join(subdir).join(FILE_NAME);
    let outcome = match std::fs::read_to_string(&file) {
        Ok(existing) if existing == source => Outcome::Unchanged(file.clone()),
        Ok(existing) if !existing.contains(MARKER) && !force => {
            bail!(
                "{} exists and was not written by speedrun; use --force to replace it",
                file.display()
            )
        }
        Ok(_) => Outcome::Upgraded(file.clone()),
        Err(_) => Outcome::Installed(file.clone()),
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
        Err(_) => json!({}),
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
        let file = base.join("extensions").join(FILE_NAME);
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
        let home = Path::new("/h");
        let proj = Path::new("/p");
        assert_eq!(
            target_dir(Host::Opencode, None, home, None),
            Path::new("/h/.config/opencode")
        );
        assert_eq!(
            target_dir(Host::Opencode, None, home, Some(Path::new("/x"))),
            Path::new("/x/opencode")
        );
        assert_eq!(
            target_dir(Host::Opencode, Some(proj), home, None),
            Path::new("/p/.opencode")
        );
        assert_eq!(target_dir(Host::Pi, None, home, None), Path::new("/h/.pi/agent"));
        assert_eq!(
            target_dir(Host::Omp, Some(proj), home, None),
            Path::new("/p/.omp")
        );
    }

    #[test]
    fn shipped_adapters_carry_the_marker() {
        assert!(OPENCODE_PLUGIN.contains(MARKER) && PI_EXTENSION.contains(MARKER));
    }
}
