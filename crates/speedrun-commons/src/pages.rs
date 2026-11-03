//! The public pages (COMMONS.md §20): server-rendered HTML from `maud`, which
//! escapes every interpolated value by construction and emits no script. This
//! module owns the decisions a page makes — the layout, the security policy,
//! the clock and trust spellings, and how a publication names the person it is
//! shown as — so the HTTP module only maps a `Page` onto a response.

use anyhow::anyhow;
use axum::http::StatusCode;
use chrono::{DateTime, SecondsFormat, Utc};
use ed25519_dalek::SigningKey;
use maud::{DOCTYPE, Markup, html};
use speedrun_core::commons::{Judgment, PublicPolicyStatus, TreeHead};
use speedrun_core::{RunView, Trust, sign};

use crate::log;
use crate::record;
use crate::store::{Holder, ListedPublication, Store, StoredPublication};

/// The security policy every page carries (§20).
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'";

/// One rendered page: its status and its escaped body.
pub struct Page {
    pub status: StatusCode,
    pub body: Markup,
}

impl Page {
    fn ok(body: Markup) -> Self {
        Self {
            status: StatusCode::OK,
            body,
        }
    }

    fn not_found(body: Markup) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body,
        }
    }
}

/// How a publication names the person it is shown as.
enum PersonCell {
    Linked(String),
    /// Linked to a person whose login someone else has since taken.
    FormerLogin(u64),
    Unlinked(Option<String>),
}

struct ListedView {
    run_id: String,
    digest: String,
    tier: u8,
    received: String,
    person: PersonCell,
    category: Option<String>,
    result: &'static str,
    timed: Option<String>,
}

struct DeviceRow {
    fingerprint: String,
    linked: String,
}

struct MilestoneRow {
    title: String,
    required: &'static str,
    claimed: &'static str,
    granted: &'static str,
    problems: String,
    verification: String,
}

/// `GET /`: the 50 newest publications, newest first.
pub async fn front(store: &Store) -> anyhow::Result<Page> {
    let listed = store.recent_publications(50).await?;
    let views = listed_views(store, &listed).await?;
    let content = html! {
        h1 { "Recent publications" }
        p { "The 50 most recently received publications." }
        (publication_table(&views))
    };
    Ok(Page::ok(layout("Recent publications", content)))
}

/// `GET /runs/{run_id}`: the run's latest publication.
pub async fn run(store: &Store, key: &SigningKey, base: &str, run_id: &str) -> anyhow::Result<Page> {
    let stored = match store.latest_publication_for_run(run_id).await? {
        Some(stored) => stored,
        None => return Ok(not_found_page("No such run")),
    };
    render_publication(store, key, base, stored).await
}

/// `GET /publications/{digest}`: a specific publication, even after a later one.
pub async fn publication(store: &Store, key: &SigningKey, base: &str, digest: &str) -> anyhow::Result<Page> {
    let stored = match store.publication(digest).await? {
        Some(stored) => stored,
        None => return Ok(not_found_page("No such publication")),
    };
    render_publication(store, key, base, stored).await
}

async fn render_publication(
    store: &Store,
    key: &SigningKey,
    base: &str,
    stored: StoredPublication,
) -> anyhow::Result<Page> {
    let received = timestamp(stored.received_at);
    let publisher = stored.publisher_pubkey.clone();
    let published = record::published_run(stored)
        .map_err(|error| anyhow!("a stored publication cannot be shown: {error:?}"))?;
    let person = match publisher.as_deref() {
        Some(pubkey) => store.person_for_device(pubkey).await?,
        None => None,
    };
    let cell = person_cell(person.as_ref(), publisher.as_deref())?;
    let head_body = match store.latest_tree_head().await? {
        Some(body) => body,
        None => return Err(anyhow!("the log holds publications but no tree head")),
    };
    let head: TreeHead = serde_json::from_str(&head_body)?;
    let leaves = store.leaf_hashes().await?;
    let inclusion = log::proof(&leaves, published.leaf_index, Some(head.tree_size))
        .map_err(|error| anyhow!("the leaf has no proof against the latest head: {error:?}"))?;
    let commons_pubkey = sign::pubkey_hex(key);
    let rows = match &published.view {
        Some(view) => milestone_rows(view, &published.judgment)?,
        None => Vec::new(),
    };
    let history = store.publication_history(&published.bundle.run_id).await?;
    let content = html! {
        h1 { "Run" }
        p { "Run ID " code { (published.bundle.run_id) } }
        p { "Tier " (published.tier) " · received " (received) " · " (person_markup(&cell)) }
        @if let Some(view) = &published.view {
            p { "Category " code { (view.category_id) "@" (view.category_version) } }
            h2 { "Result" }
            p { "Local completion: " (match published.judgment.local_completed { Some(true) => "completed", Some(false) => "not completed", None => "unavailable" }) }
            p { "Public policy: " (policy_markup(published.judgment.public_policy.as_ref())) }
            h2 { "Clocks" }
            p { "Timed time excludes explicit pauses." }
            div class="table-wrap" tabindex="0" aria-label="Clocks; scroll horizontally for more columns" {
                table {
                    thead { tr { th { "Clock" } th { "Elapsed" } } }
                    tbody {
                        tr { td { "wall" } td { (hms(view.timing.wall_ms)) } }
                        tr { td { "timed" } td { (hms(view.timing.timed_ms)) } }
                        tr { td { "agent" } td { (hms(view.timing.agent_ms)) } }
                        tr { td { "human" } td { (hms(view.timing.human_ms)) } }
                    }
                }
            }
            h2 { "Milestones" }
            div class="table-wrap" tabindex="0" aria-label="Milestones; scroll horizontally for more columns" {
                table {
                    thead {
                        tr {
                            th { "Milestone" }
                            th { "Required" }
                            th { "Claimed" }
                            th { "Granted" }
                            th { "Verification scope and status" }
                            th { "Problems" }
                        }
                    }
                    tbody {
                        @for row in &rows {
                            tr {
                                td { (row.title) }
                                td { (trust_markup(row.required)) }
                                td { (trust_markup(row.claimed)) }
                                td { (trust_markup(row.granted)) }
                                td { (row.verification) }
                                td { (row.problems) }
                            }
                        }
                    }
                }
            }
        } @else {
            p { "Tier 0 publishes chain heads only; category, clocks, and milestone evidence are unavailable." }
            h2 { "Chain heads" }
            div class="table-wrap" {
                table {
                    thead { tr { th { "Device" } th { "Seq" } th { "Hash" } } }
                    tbody {
                        @for chain in &published.bundle.chain_heads {
                            tr {
                                td { (chain.device_id) }
                                td { (chain.seq) }
                                td { code { (chain.hash) } }
                            }
                        }
                    }
                }
            }
        }
        h2 { "Publication history" }
        p { "Each link identifies a specific publication. The transparency proof below uses the latest tree head." }
        ol {
            @for item in &history {
                li { a href=(format!("/publications/{}", item.digest)) { (timestamp(item.received_at)) " · tier " (item.tier) } }
            }
        }
        details {
        summary { "Record and verification details" }
        div class="table-wrap" {
            table {
                tbody {
                    tr {
                        th { "Bundle" }
                        td {
                            a href=(format!("/v1/bundles/{}", published.bundle_digest)) {
                                (published.bundle_digest)
                            }
                        }
                    }
                    tr { th { "Judgment signature" } td { code { (published.judgment.signature) } } }
                    tr { th { "Commons key" } td { code { (commons_pubkey) } } }
                    tr { th { "Leaf index" } td { (published.leaf_index) } }
                    tr { th { "Tree size" } td { (head.tree_size) } }
                    tr { th { "Tree root" } td { code { (head.root_hash) } } }
                    tr {
                        th { "Audit path" }
                        td {
                            @for step in &inclusion.audit_path {
                                div { code { (step) } }
                            }
                        }
                    }
                }
            }
        }
        h2 { "Fetch" }
        p { code { "curl " (base) "/v1/bundles/" (published.bundle_digest) } }
        p { code { "curl " (base) "/v1/log/proof/" (published.leaf_index) "?tree_size=" (head.tree_size) } }
        }
    };
    Ok(Page::ok(layout(&published.bundle.run_id, content)))
}

/// `GET /people/{login}`: a person, their devices and their publications.
pub async fn person(store: &Store, login: &str) -> anyhow::Result<Page> {
    let found = match store.person_record(login).await? {
        Some(found) => found,
        None => return Ok(not_found_page("No such person")),
    };
    let github_id = i64::try_from(found.person.github_id)?;
    let devices = store.person_devices(github_id).await?;
    let mut device_rows = Vec::with_capacity(devices.len());
    for device in &devices {
        let fingerprint = sign::fingerprint(&device.link.claim.device_pubkey)
            .map_err(|error| anyhow!("a stored link names a malformed device key: {error}"))?;
        device_rows.push(DeviceRow {
            fingerprint,
            linked: timestamp(device.linked_at),
        });
    }
    let mut views = Vec::with_capacity(found.publications.len());
    for summary in &found.publications {
        let mut view = ListedView {
            run_id: summary.run_id.clone(),
            digest: summary.bundle_digest.clone(),
            tier: summary.tier,
            received: timestamp(summary.received_at),
            person: PersonCell::Linked(found.person.login.clone()),
            category: None,
            result: "Unavailable",
            timed: None,
        };
        populate_listing(store, &mut view).await?;
        views.push(view);
    }
    let content = html! {
        h1 { (found.person.login) }
        p { "GitHub id " code { (found.person.github_id) } }
        h2 { "Linked devices" }
        @if device_rows.is_empty() {
            p { "No linked devices." }
        } @else {
            div class="table-wrap" {
                table {
                    thead { tr { th { "Fingerprint" } th { "Linked" } } }
                    tbody {
                        @for row in &device_rows {
                            tr { td { code { (row.fingerprint) } } td { (row.linked) } }
                        }
                    }
                }
            }
        }
        h2 { "Publications" }
        (publication_table(&views))
    };
    Ok(Page::ok(layout(&found.person.login, content)))
}

async fn listed_views(store: &Store, listed: &[ListedPublication]) -> anyhow::Result<Vec<ListedView>> {
    let mut views = Vec::with_capacity(listed.len());
    for item in listed {
        let mut view = ListedView {
            run_id: item.summary.run_id.clone(),
            digest: item.summary.bundle_digest.clone(),
            tier: item.summary.tier,
            received: timestamp(item.summary.received_at),
            person: person_cell(item.person.as_ref(), item.publisher_pubkey.as_deref())?,
            category: None,
            result: "Unavailable",
            timed: None,
        };
        populate_listing(store, &mut view).await?;
        views.push(view);
    }
    Ok(views)
}

async fn populate_listing(store: &Store, view: &mut ListedView) -> anyhow::Result<()> {
    let stored = store
        .publication(&view.digest)
        .await?
        .ok_or_else(|| anyhow!("listed publication is missing"))?;
    let published = record::published_run(stored)
        .map_err(|error| anyhow!("a stored publication cannot be shown: {error:?}"))?;
    if let Some(run) = published.view {
        view.category = Some(format!("{}@{}", run.category_id, run.category_version));
        view.timed = Some(hms(run.timing.timed_ms));
        view.result = match published.judgment.local_completed {
            Some(true) => "Completed",
            Some(false) => "Not completed",
            None => "Unavailable",
        };
    }
    Ok(())
}

fn person_cell(person: Option<&Holder>, publisher: Option<&str>) -> anyhow::Result<PersonCell> {
    match person {
        Some(Holder {
            login: Some(login), ..
        }) => Ok(PersonCell::Linked(login.clone())),
        Some(Holder {
            github_id,
            login: None,
        }) => Ok(PersonCell::FormerLogin(*github_id)),
        None => match publisher {
            Some(pubkey) => {
                let fingerprint = sign::fingerprint(pubkey)
                    .map_err(|error| anyhow!("a stored publisher key is malformed: {error}"))?;
                Ok(PersonCell::Unlinked(Some(fingerprint)))
            }
            None => Ok(PersonCell::Unlinked(None)),
        },
    }
}

fn person_markup(cell: &PersonCell) -> Markup {
    match cell {
        PersonCell::Linked(login) => html! {
            a href=(format!("/people/{login}")) { (login) }
        },
        PersonCell::FormerLogin(github_id) => html! {
            "github id " (github_id)
        },
        PersonCell::Unlinked(Some(fingerprint)) => html! {
            "unlinked device " code { (fingerprint) }
        },
        PersonCell::Unlinked(None) => html! {
            "unlinked device"
        },
    }
}

fn publication_table(views: &[ListedView]) -> Markup {
    html! {
        @if views.is_empty() {
            p { "No publications yet." }
        } @else {
            div class="table-wrap" tabindex="0" aria-label="Publications; scroll horizontally for more columns" {
                table {
                    thead {
                        tr { th { "Run" } th { "Result" } th { "Timed" } th { "Tier" } th { "Received" } th { "Person" } }
                    }
                    tbody {
                        @for view in views {
                            tr {
                                td { a href=(format!("/runs/{}", view.run_id)) { (view.category.as_deref().unwrap_or("Run details")) } br; a href=(format!("/publications/{}", view.digest)) { "This publication" } details { summary { "Run ID" } code { (view.run_id) } } }
                                td { (view.result) }
                                td { (view.timed.as_deref().unwrap_or("Unavailable")) }
                                td { (view.tier) }
                                td { (view.received) }
                                td { (person_markup(&view.person)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn milestone_rows(view: &RunView, judgment: &Judgment) -> anyhow::Result<Vec<MilestoneRow>> {
    let mut rows = Vec::with_capacity(judgment.milestones.len());
    for judged in &judgment.milestones {
        let milestone = match view
            .milestones
            .iter()
            .find(|milestone| milestone.milestone_id == judged.milestone_id)
        {
            Some(milestone) => milestone,
            None => {
                return Err(anyhow!(
                    "the judgment names milestone {} the run does not hold",
                    judged.milestone_id
                ));
            }
        };
        rows.push(MilestoneRow {
            title: milestone.title.clone(),
            required: trust_name(Some(milestone.required_trust)),
            claimed: trust_name(judged.claimed),
            granted: trust_name(judged.granted),
            problems: if judged.problems.is_empty() {
                "None reported".into()
            } else {
                judged.problems.join("; ")
            },
            verification: judged.verification.as_ref().map_or_else(
                || "Not reported".into(),
                |report| {
                    use speedrun_core::api::{VerificationScope, VerificationStatus};
                    let scope = match report.scope {
                        VerificationScope::Local => "Local",
                        VerificationScope::Public => "Public",
                    };
                    let status = match report.status {
                        VerificationStatus::Verified => "Verified",
                        VerificationStatus::Failed => "Failed",
                        VerificationStatus::Unavailable => "Unavailable",
                    };
                    format!("{scope} · {status}")
                },
            ),
        });
    }
    Ok(rows)
}

fn policy_markup(policy: Option<&PublicPolicyStatus>) -> Markup {
    match policy {
        Some(PublicPolicyStatus::Completed) => html! { "completed" },
        Some(PublicPolicyStatus::Incomplete) => html! { "incomplete" },
        Some(PublicPolicyStatus::Unavailable { reason }) => html! { "unavailable: " (reason) },
        None => html! { "unavailable" },
    }
}

fn trust_name(trust: Option<Trust>) -> &'static str {
    match trust {
        Some(Trust::SelfDeclared) => "self_declared",
        Some(Trust::AgentClaimed) => "agent_claimed",
        Some(Trust::EvidenceVerified) => "evidence_verified",
        None => "none",
    }
}

fn trust_markup(code: &str) -> Markup {
    let label = match code {
        "self_declared" => "Self declared",
        "agent_claimed" => "Agent claimed",
        "evidence_verified" => "Evidence verified",
        _ => "None",
    };
    html! { span title=(code) { (label) } }
}

fn hms(ms: i64) -> String {
    let seconds = ms / 1000;
    let hours = seconds / 3600;
    let minutes = seconds / 60 % 60;
    let secs = seconds % 60;
    format!("{hours}:{minutes:02}:{secs:02}")
}

fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn not_found_page(message: &str) -> Page {
    let content = html! {
        h1 { "Not found" }
        p { (message) }
        p { a href="/" { "Recent publications" } }
    };
    Page::not_found(layout("Not found", content))
}

fn layout(title: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style {
                    "* { box-sizing: border-box; } body { margin: 0 auto; padding: 1rem; max-width: 72rem; font-family: system-ui, sans-serif; line-height: 1.5; color: #17212b; } header { border-bottom: 1px solid #cbd5df; padding-bottom: 1rem; } main { min-width: 0; } h1 { font-size: 1.6rem; overflow-wrap: anywhere; } h2 { font-size: 1.2rem; margin-top: 2rem; } a { color: #125798; } a:focus-visible, summary:focus-visible { outline: 3px solid #b46000; outline-offset: 3px; } .table-wrap { overflow-x: auto; max-width: 100%; } table { border-collapse: collapse; width: 100%; } th, td { text-align: left; padding: 0.6rem; vertical-align: top; border-bottom: 1px solid #dbe2e8; } th { background: #f2f5f8; } code, pre { overflow-wrap: anywhere; white-space: pre-wrap; } summary { cursor: pointer; font-weight: 600; padding: 0.75rem 0; } li { overflow-wrap: anywhere; }"
                }
            }
            body {
                header { nav aria-label="Main navigation" { a href="/" { "Agent Speedrun Commons" } } }
                main { (content) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_rows_link_to_the_specific_record() {
        let views = vec![ListedView {
            run_id: "run-1".into(),
            digest: "digest-1".into(),
            tier: 1,
            received: "now".into(),
            person: PersonCell::Unlinked(None),
            category: Some("any-percent@2".into()),
            result: "Completed",
            timed: Some("0:01:05".into()),
        }];
        let body = publication_table(&views).into_string();
        assert!(body.contains("href=\"/runs/run-1\""));
        assert!(body.contains("href=\"/publications/digest-1\""));
        assert!(body.contains("any-percent@2"));
        assert!(body.contains("0:01:05"));
        assert!(
            publication_table(&[])
                .into_string()
                .contains("No publications yet.")
        );
    }

    #[test]
    fn public_policy_unavailability_is_explicit_and_escaped() {
        let policy = PublicPolicyStatus::Unavailable {
            reason: "<unavailable>".into(),
        };
        let body = policy_markup(Some(&policy)).into_string();
        assert!(body.contains("unavailable: &lt;unavailable&gt;"));
        assert_eq!(policy_markup(None).into_string(), "unavailable");
    }
}
