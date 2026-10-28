use std::path::PathBuf;

/// Default daemon listen address. Loopback only.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7477";

/// Where the daemon keeps its database and auth token.
///
/// `$SPEEDRUN_HOME`, else `$XDG_DATA_HOME/agentspeedrun`, else `~/.local/share/agentspeedrun`.
pub fn data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SPEEDRUN_HOME") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(p).join("agentspeedrun");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/agentspeedrun")
}

pub fn token_path() -> PathBuf {
    data_dir().join("token")
}

/// Read-only token for live displays: sees timing and milestones, never prompts.
pub fn display_token_path() -> PathBuf {
    data_dir().join("display-token")
}

pub fn daemon_addr() -> String {
    std::env::var("SPEEDRUN_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string())
}

/// Per-project config, committed alongside the code.
pub const PROJECT_FILE: &str = ".speedrun/project.json";
