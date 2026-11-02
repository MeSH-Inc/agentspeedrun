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

/// Ed25519 device key (hex seed, mode 0600). Generated on first start.
pub fn device_key_path() -> PathBuf {
    data_dir().join("device.key")
}

/// Read-only token for live displays: sees timing and milestones, never prompts.
pub fn display_token_path() -> PathBuf {
    data_dir().join("display-token")
}

/// Token agent tools (the MCP server, `speedrun tool`) send. It may append a
/// few payload kinds and marks a claim as the agent's; it is never a read token.
pub fn agent_token_path() -> PathBuf {
    data_dir().join("agent-token")
}

/// Verifier credentials (`stripe`, `github`), one file each, mode 0600.
pub fn secrets_dir() -> PathBuf {
    data_dir().join("secrets")
}

/// The address `SPEEDRUN_ADDR` names, when it is set and not empty. The
/// daemon listens there and clients use it before `$DATA_DIR/addr`.
pub fn configured_addr() -> Option<String> {
    std::env::var_os("SPEEDRUN_ADDR")
        .map(|addr| addr.to_string_lossy().trim().to_string())
        .filter(|addr| !addr.is_empty())
}

/// Where the daemon records the `host:port` it bound, mode 0600. Naming the
/// file is core's job; reading it is the client's.
pub fn addr_path() -> PathBuf {
    data_dir().join("addr")
}

/// Per-project config, committed alongside the code.
pub const PROJECT_FILE: &str = ".speedrun/project.json";
