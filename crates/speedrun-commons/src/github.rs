//! Where the commons reaches GitHub, and the OAuth app it links persons with
//! (COMMONS.md §20). The commons runs GitHub's device flow on a device's
//! behalf, reads `/user` once with the token it gets, and keeps nothing but
//! the person: the token never leaves the request that received it.

use std::time::Duration;

use serde::Deserialize;
use speedrun_core::commons::Person;

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

const ACCEPT: &str = "application/json";
const USER_AGENT: &str = "speedrun-commons";

/// GitHub's device-code answer: the code a person enters and the limits
/// GitHub sets on polling it.
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_seconds: u64,
    pub expires_in: u64,
}

/// What one token poll answered. A token is used at once and never stored.
pub enum TokenPoll {
    Pending,
    SlowDown,
    Refused,
    Token(String),
}

/// A GitHub request failed; the commons answers 502 and changes nothing.
#[derive(Debug)]
pub enum GitHubError {
    /// The request never reached GitHub, or the transport failed.
    Unreachable,
    /// GitHub answered a status the device flow does not use.
    Status(u16),
    /// GitHub's answer is not the JSON the device flow expects.
    Malformed(String),
}

impl std::fmt::Display for GitHubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable => write!(f, "GitHub is unreachable"),
            Self::Status(status) => write!(f, "GitHub answered {status}"),
            Self::Malformed(message) => write!(f, "GitHub's answer is malformed: {message}"),
        }
    }
}

/// Ask GitHub for the code a person enters to authorize this app. The scope is
/// empty because linking only reads `/user`.
pub fn request_device_code(config: &GitHubConfig, client_id: &str) -> Result<DeviceCode, GitHubError> {
    let url = format!("{}/login/device/code", config.web_url);
    let body = post_form(&url, &[("client_id", client_id), ("scope", "")])?;
    let answer: DeviceCodeAnswer =
        serde_json::from_str(&body).map_err(|error| GitHubError::Malformed(error.to_string()))?;
    Ok(DeviceCode {
        device_code: answer.device_code,
        user_code: answer.user_code,
        verification_uri: answer.verification_uri,
        interval_seconds: answer.interval,
        expires_in: answer.expires_in,
    })
}

/// Poll GitHub once for the token that belongs to `device_code`.
pub fn poll_token(
    config: &GitHubConfig,
    client_id: &str,
    device_code: &str,
) -> Result<TokenPoll, GitHubError> {
    let url = format!("{}/login/oauth/access_token", config.web_url);
    let body = post_form(
        &url,
        &[
            ("client_id", client_id),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ],
    )?;
    let answer: TokenAnswer =
        serde_json::from_str(&body).map_err(|error| GitHubError::Malformed(error.to_string()))?;
    if let Some(error) = answer.error {
        return match error.as_str() {
            "authorization_pending" => Ok(TokenPoll::Pending),
            "slow_down" => Ok(TokenPoll::SlowDown),
            "expired_token" | "access_denied" => Ok(TokenPoll::Refused),
            _ => Err(GitHubError::Malformed(error)),
        };
    }
    match answer.access_token {
        Some(token) => Ok(TokenPoll::Token(token)),
        None => Err(GitHubError::Malformed("no token and no error".into())),
    }
}

/// Read the person from `/user` with `token`. The token is only ever a
/// parameter of this call: it is not stored, logged, returned or named in an
/// error.
pub fn fetch_user(config: &GitHubConfig, token: &str) -> Result<Person, GitHubError> {
    let url = format!("{}/user", config.api_url);
    let authorization = format!("Bearer {token}");
    let agent = agent();
    let response = agent
        .get(url.as_str())
        .set("Accept", ACCEPT)
        .set("User-Agent", USER_AGENT)
        .set("Authorization", &authorization)
        .call()
        .map_err(github_error)?;
    let body = response
        .into_string()
        .map_err(|error| GitHubError::Malformed(error.to_string()))?;
    let answer: UserAnswer =
        serde_json::from_str(&body).map_err(|error| GitHubError::Malformed(error.to_string()))?;
    Ok(Person {
        github_id: answer.id,
        login: answer.login,
    })
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build()
}

fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<String, GitHubError> {
    let response = agent()
        .post(url)
        .set("Accept", ACCEPT)
        .set("User-Agent", USER_AGENT)
        .send_form(fields)
        .map_err(github_error)?;
    response
        .into_string()
        .map_err(|error| GitHubError::Malformed(error.to_string()))
}

fn github_error(error: ureq::Error) -> GitHubError {
    match error {
        ureq::Error::Status(status, _) => GitHubError::Status(status),
        ureq::Error::Transport(_) => GitHubError::Unreachable,
    }
}

#[derive(Deserialize)]
struct DeviceCodeAnswer {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

#[derive(Deserialize)]
struct TokenAnswer {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct UserAnswer {
    id: u64,
    login: String,
}
