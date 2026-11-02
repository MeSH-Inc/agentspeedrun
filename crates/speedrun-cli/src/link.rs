//! `speedrun link`: link this device's key to a GitHub account through a
//! commons (COMMONS.md §20). The commons runs GitHub's device flow, so the
//! GitHub token never reaches this machine. The command carries only the two
//! signatures: the daemon signs the claim, and the commons countersigns it.

use std::time::Duration;

use anyhow::{Result, bail};
use chrono::Utc;
use speedrun_core::commons::{
    CommonsHealth, DeviceKey, DeviceSignature, LINK_FORMAT, Link, LinkClaim, LinkConfirm, LinkStart,
    LinkStarted, LinkState, Person,
};

use crate::client::Client;
use crate::publish::Commons;

pub fn run(client: &Client, to: Option<&str>) -> Result<()> {
    // Resolve the commons first: a command that cannot name one sends
    // nothing to the daemon.
    let commons = Commons::resolve(to)?;
    let device: DeviceKey = client.get("/v1/device")?;
    let started: LinkStarted = commons.post(
        "/v1/links",
        &LinkStart {
            device_pubkey: device.device_pubkey.clone(),
        },
    )?;
    println!(
        "open {} and enter {}",
        started.verification_uri, started.user_code
    );
    let person = wait_for_person(&commons, &started)?;
    let health: CommonsHealth = commons.get("/v1/health")?;
    let claim = LinkClaim {
        format: LINK_FORMAT,
        link_id: started.link_id.clone(),
        device_pubkey: device.device_pubkey,
        github_id: person.github_id,
        login: person.login,
        commons_pubkey: health.commons_pubkey,
        issued_at: Utc::now(),
    };
    let signature: DeviceSignature = client.post("/v1/link-claims", &serde_json::to_value(&claim)?)?;
    let confirm_path = format!("/v1/links/{}/confirm", claim.link_id);
    let confirmed: Link = commons.post(
        &confirm_path,
        &LinkConfirm {
            claim,
            device_signature: signature.device_signature,
        },
    )?;
    println!(
        "linked {} (github id {}) to device {}",
        confirmed.claim.login, confirmed.claim.github_id, device.device_id
    );
    Ok(())
}

/// Poll the commons until GitHub authorizes the link, or the link expires.
fn wait_for_person(commons: &Commons, started: &LinkStarted) -> Result<Person> {
    loop {
        if Utc::now() >= started.expires_at {
            bail!(
                "the link expired at {} before it was authorized",
                started.expires_at.to_rfc3339()
            );
        }
        let state: LinkState = commons.post(
            &format!("/v1/links/{}/poll", started.link_id),
            &serde_json::json!({}),
        )?;
        match state {
            LinkState::Authorized { person } => return Ok(person),
            LinkState::Pending => {}
        }
        std::thread::sleep(Duration::from_secs(started.interval_seconds));
    }
}
