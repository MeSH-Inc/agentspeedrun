//! `speedrun publish`: carry a signed bundle from the daemon to a commons.
//!
//! Two servers, two trust decisions. The daemon must prove this data dir's
//! token and is the only place the full token goes; the commons is a
//! stranger that receives the bundle over a plain request and never sees a
//! speedrun token. The destination is resolved before any daemon traffic, so
//! a command that cannot name a commons sends nothing at all.

use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use speedrun_core::Trust;
use speedrun_core::bundle::Bundle;
use speedrun_core::commons::{MilestoneJudgment, PublishReceipt};

use crate::client::Client;

/// A commons to talk to: its base URL, and the plain requests it accepts.
/// The commons is a different server, so nothing here carries a speedrun
/// token. The command line and the environment are read once, in `resolve`;
/// the rest of a command sees only this value.
pub(crate) struct Commons {
    destination: Destination,
}

/// Where a commons lives. Any trailing slash on the base is absorbed by
/// `url`, so callers pass only the route.
struct Destination(String);

impl Commons {
    /// `--to`, else `SPEEDRUN_COMMONS_URL`. Neither is an error, not a
    /// fallback to some default server.
    pub(crate) fn resolve(to: Option<&str>) -> Result<Self> {
        Ok(Self {
            destination: Destination::resolve(to)?,
        })
    }

    /// One GET, decoded as JSON.
    pub(crate) fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.destination.url(path);
        let response = ureq::get(&url).call().map_err(|e| refusal(&url, e))?;
        response
            .into_json()
            .with_context(|| format!("reading the answer from {url}"))
    }

    /// One POST of a JSON body, decoded as JSON. Any 2xx status is success,
    /// so a 202 from a poll parses like a 200.
    pub(crate) fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let url = self.destination.url(path);
        let response = ureq::post(&url).send_json(body).map_err(|e| refusal(&url, e))?;
        response
            .into_json()
            .with_context(|| format!("reading the answer from {url}"))
    }
}

impl Destination {
    /// `--to`, else `SPEEDRUN_COMMONS_URL`. Neither is an error, not a
    /// fallback to some default server.
    fn resolve(to: Option<&str>) -> Result<Self> {
        if let Some(url) = to.map(str::trim).filter(|url| !url.is_empty()) {
            return Ok(Self(url.to_string()));
        }
        match std::env::var("SPEEDRUN_COMMONS_URL") {
            Ok(url) if !url.trim().is_empty() => Ok(Self(url.trim().to_string())),
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                bail!("no commons address: pass --to <url> or set SPEEDRUN_COMMONS_URL")
            }
            Err(e @ std::env::VarError::NotUnicode(_)) => {
                Err(anyhow::anyhow!("SPEEDRUN_COMMONS_URL is not valid unicode: {e}"))
            }
        }
    }

    /// `path`, which starts with `/`, on this base.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.0.trim_end_matches('/'))
    }
}

/// A non-2xx answer, or no answer, as the command reports it.
fn refusal(url: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, response) => {
            let reason = match response.into_string() {
                Ok(reason) => reason,
                Err(e) => format!("unreadable refusal body: {e}"),
            };
            anyhow::anyhow!("the commons at {url} refused the request (HTTP {code}): {reason}")
        }
        ureq::Error::Transport(transport) => {
            anyhow::anyhow!("talking to the commons at {url}: {transport}")
        }
    }
}

/// Fetch `run_id`'s bundle at `tier` from the daemon, post it to the
/// resolved commons, then report the receipt.
pub fn run(client: &Client, tier: u8, run_id: &str, to: Option<&str>) -> Result<()> {
    if tier > 2 {
        bail!("tier must be 0, 1 or 2");
    }
    // Resolve the destination first: an unnamed commons leaves the daemon
    // untouched, so nothing is fetched or sent.
    let commons = Commons::resolve(to)?;
    let path = format!("/v1/runs/{}/bundle?tier={tier}", path_segment(run_id));
    let bundle: Bundle = client.get(&path)?;
    let receipt: PublishReceipt = commons.post("/v1/bundles", &bundle)?;
    print_receipt(&receipt);
    Ok(())
}

/// `text` as one URL path segment: everything but RFC 3986's unreserved
/// characters is percent-encoded, so a `/`, `?` or `#` in a run id cannot
/// reshape the request.
fn path_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn print_receipt(receipt: &PublishReceipt) {
    println!("published {}", receipt.bundle_digest);
    println!(
        "leaf {} of {}",
        receipt.inclusion.leaf_index, receipt.inclusion.tree_size
    );
    for judgment in &receipt.judgment.milestones {
        println!("{}", milestone_line(judgment));
    }
}

fn milestone_line(judgment: &MilestoneJudgment) -> String {
    let granted = match judgment.granted {
        Some(trust) => trust_name(trust),
        None => "none",
    };
    let mut line = format!("{}: {granted}", judgment.milestone_id);
    if !judgment.problems.is_empty() {
        line.push_str(&format!(" ({})", judgment.problems.join("; ")));
    }
    line
}

fn trust_name(trust: Trust) -> &'static str {
    match trust {
        Trust::SelfDeclared => "self_declared",
        Trust::AgentClaimed => "agent_claimed",
        Trust::EvidenceVerified => "evidence_verified",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_run_id_stays_one_path_segment() {
        assert_eq!(super::path_segment("01JABC"), "01JABC");
        assert_eq!(super::path_segment("a/b?c#d e"), "a%2Fb%3Fc%23d%20e");
    }
}
