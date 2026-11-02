//! Where the commons reaches GitHub, and the OAuth app it links persons with
//! (COMMONS.md §20). The commons runs GitHub's device flow on a device's
//! behalf, reads `/user` once with the token it gets, and keeps nothing but
//! the person: the token never leaves the request that received it.

/// GitHub's addresses and the OAuth app's client id. Tests point both
/// addresses at a stand-in server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubConfig {
    /// Base of the device-flow endpoints (`/login/device/code`,
    /// `/login/oauth/access_token`). `SPEEDRUN_GITHUB_URL`, else `https://github.com`.
    pub web_url: String,
    /// Base of the REST API (`/user`). `SPEEDRUN_GITHUB_API`, else `https://api.github.com`.
    pub api_url: String,
    /// The OAuth app's client id, `SPEEDRUN_GITHUB_CLIENT_ID`. Without it the
    /// commons cannot link anyone and answers `POST /v1/links` with 503.
    pub client_id: Option<String>,
}

impl GitHubConfig {
    pub fn from_env() -> Self {
        Self {
            web_url: setting("SPEEDRUN_GITHUB_URL", "https://github.com"),
            api_url: setting("SPEEDRUN_GITHUB_API", "https://api.github.com"),
            client_id: match std::env::var("SPEEDRUN_GITHUB_CLIENT_ID") {
                Ok(id) if !id.trim().is_empty() => Some(id.trim().to_string()),
                _ => None,
            },
        }
    }
}

/// An address from the environment; unset or empty means GitHub's own.
fn setting(name: &str, github: &str) -> String {
    match std::env::var(name) {
        Ok(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_string(),
        _ => github.to_string(),
    }
}
