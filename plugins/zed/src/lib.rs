//! agentspeedrun for Zed: exposes `speedrun mcp` as a context server, giving
//! Zed's agent the milestone tools. Zed's extension API cannot observe
//! prompts; agents Zed runs over ACP (Claude Code, Copilot CLI) are timed by
//! their own hooks. Source: plugins/zed in mesh-inc/agentspeedrun.

use serde_json::Value;
use zed_extension_api::{self as zed, Command, ContextServerConfiguration, ContextServerId, Project, Result};

const INSTALL: &str = "Install the `speedrun` CLI and make sure Zed can find it \
(set `speedrun_path` if it is not on Zed's `PATH`). Then run `speedrun init` in each \
project you want timed.\n\nThe server finds the project from `project_dir` if set, \
otherwise from the directory Zed starts it in.";

const SETTINGS_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "speedrun_path": { "type": "string", "description": "Path to the speedrun CLI. Default: speedrun on PATH." },
    "project_dir": { "type": "string", "description": "Absolute path of the project to serve, if Zed does not start the server in it." }
  },
  "additionalProperties": false
}"#;

const DEFAULT_SETTINGS: &str = r#"{
  "speedrun_path": "speedrun"
}"#;

/// The command to launch for the given extension settings. Kept free of
/// Zed types so it can be unit tested on the host.
pub fn server_command(settings: Option<&Value>) -> (String, Vec<String>) {
    let get = |key: &str| {
        settings
            .and_then(|s| s.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let path = get("speedrun_path").unwrap_or("speedrun").to_string();
    let mut args = vec!["mcp".to_string()];
    if let Some(dir) = get("project_dir") {
        args.extend(["--project-dir".to_string(), dir.to_string()]);
    }
    (path, args)
}

struct AgentSpeedrun;

impl zed::Extension for AgentSpeedrun {
    fn new() -> Self {
        AgentSpeedrun
    }

    fn context_server_command(&mut self, id: &ContextServerId, project: &Project) -> Result<Command> {
        let configured = zed::settings::ContextServerSettings::for_project(id.as_ref(), project)?;
        // A full `command` override in settings wins over our defaults.
        if let Some(cmd) = configured.command
            && let Some(path) = cmd.path
        {
            return Ok(Command {
                command: path,
                args: cmd.arguments.unwrap_or_else(|| vec!["mcp".into()]),
                env: cmd.env.map(|e| e.into_iter().collect()).unwrap_or_default(),
            });
        }
        let (command, args) = server_command(configured.settings.as_ref());
        Ok(Command {
            command,
            args,
            env: Vec::new(),
        })
    }

    fn context_server_configuration(
        &mut self,
        _id: &ContextServerId,
        _project: &Project,
    ) -> Result<Option<ContextServerConfiguration>> {
        Ok(Some(ContextServerConfiguration {
            installation_instructions: INSTALL.to_string(),
            settings_schema: SETTINGS_SCHEMA.to_string(),
            default_settings: DEFAULT_SETTINGS.to_string(),
        }))
    }
}

zed::register_extension!(AgentSpeedrun);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_to_speedrun_on_path() {
        assert_eq!(server_command(None), ("speedrun".into(), vec!["mcp".into()]));
        assert_eq!(
            server_command(Some(&json!({ "speedrun_path": " " }))).0,
            "speedrun"
        );
    }

    #[test]
    fn honors_path_and_project_dir() {
        let (cmd, args) = server_command(Some(&json!({
            "speedrun_path": "/opt/bin/speedrun",
            "project_dir": "/home/me/app"
        })));
        assert_eq!(cmd, "/opt/bin/speedrun");
        assert_eq!(args, ["mcp", "--project-dir", "/home/me/app"]);
    }

    #[test]
    fn schema_and_defaults_are_valid_json() {
        let schema: Value = serde_json::from_str(SETTINGS_SCHEMA).unwrap();
        let defaults: Value = serde_json::from_str(DEFAULT_SETTINGS).unwrap();
        for key in defaults.as_object().unwrap().keys() {
            assert!(schema["properties"].get(key).is_some(), "{key} not in schema");
        }
    }
}
