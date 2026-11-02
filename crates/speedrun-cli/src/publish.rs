//! `speedrun publish`: carry a signed bundle from the daemon to a commons.
//!
//! Two servers, two trust decisions. The daemon must prove this data dir's
//! token and is the only place the full token goes; the commons is a
//! stranger that receives the bundle over a plain request and never sees a
//! speedrun token. The destination is resolved before any daemon traffic, so
//! a command that cannot name a commons sends nothing at all.

use anyhow::{Context, Result, bail};
use speedrun_core::Trust;
use speedrun_core::bundle::Bundle;
use speedrun_core::commons::{MilestoneJudgment, PublishReceipt};

use crate::client::Client;

/// Where a publication goes. The command line and the environment are read
/// once here; the rest of the command sees only a resolved base URL.
struct Destination(String);

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

    /// The ingest route, with any trailing slash on the base absorbed.
    fn bundles_url(&self) -> String {
        format!("{}/v1/bundles", self.0.trim_end_matches('/'))
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
    let destination = Destination::resolve(to)?;
    let path = format!("/v1/runs/{}/bundle?tier={tier}", path_segment(run_id));
    let bundle: Bundle = client.get(&path)?;
    let receipt = post_bundle(&destination, &bundle)?;
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

/// The commons is not the daemon: a plain request, no `Authorization`
/// header, nothing but the bundle itself.
fn post_bundle(destination: &Destination, bundle: &Bundle) -> Result<PublishReceipt> {
    let url = destination.bundles_url();
    let response = match ureq::post(&url).send_json(bundle) {
        Ok(response) => response,
        // A refusal carries the commons' reason in its body; show it.
        Err(ureq::Error::Status(code, response)) => {
            let reason = response
                .into_string()
                .with_context(|| format!("reading the refusal from {url}"))?;
            bail!("the commons at {url} refused the bundle (HTTP {code}): {reason}")
        }
        Err(e) => bail!("posting the bundle to {url}: {e}"),
    };
    response
        .into_json::<PublishReceipt>()
        .with_context(|| format!("reading the receipt from {url}"))
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
