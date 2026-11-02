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
use speedrun_core::commons::{Judgment, TreeHead};
use speedrun_core::{RunView, Trust, sign};

use crate::log;
use crate::record;
use crate::store::{Holder, ListedPublication, Store};

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
    tier: u8,
    received: String,
    person: PersonCell,
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
}

/// `GET /`: the 50 newest publications, newest first.
pub async fn front(store: &Store) -> anyhow::Result<Page> {
    let listed = store.recent_publications(50).await?;
    let views = listed_views(&listed)?;
    let content = html! {
        h1 { "Recent publications" }
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
    let content = html! {
        h1 { "Run " (published.bundle.run_id) }
        p { "Tier " (published.tier) " · received " (received) " · " (person_markup(&cell)) }
        @if let Some(view) = &published.view {
            p { "Category " code { (view.category_id) "@" (view.category_version) } }
            h2 { "Clocks" }
            div class="table-wrap" {
                table {
                    thead { tr { th { "Clock" } th { "Elapsed" } } }
                    tbody {
                        tr { td { "wall" } td { (hms(view.timing.wall_ms)) } }
                        tr { td { "agent" } td { (hms(view.timing.agent_ms)) } }
                        tr { td { "human" } td { (hms(view.timing.human_ms)) } }
                    }
                }
            }
            h2 { "Milestones" }
            div class="table-wrap" {
                table {
                    thead {
                        tr {
                            th { "Milestone" }
                            th { "Required" }
                            th { "Claimed" }
                            th { "Granted" }
                            th { "Problems" }
                        }
                    }
                    tbody {
                        @for row in &rows {
                            tr {
                                td { (row.title) }
                                td { (row.required) }
                                td { (row.claimed) }
                                td { (row.granted) }
                                td { (row.problems) }
                            }
                        }
                    }
                }
            }
        } @else {
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
        h2 { "Record" }
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
        views.push(ListedView {
            run_id: summary.run_id.clone(),
            tier: summary.tier,
            received: timestamp(summary.received_at),
            person: PersonCell::Linked(found.person.login.clone()),
        });
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

fn listed_views(listed: &[ListedPublication]) -> anyhow::Result<Vec<ListedView>> {
    let mut views = Vec::with_capacity(listed.len());
    for item in listed {
        views.push(ListedView {
            run_id: item.summary.run_id.clone(),
            tier: item.summary.tier,
            received: timestamp(item.summary.received_at),
            person: person_cell(item.person.as_ref(), item.publisher_pubkey.as_deref())?,
        });
    }
    Ok(views)
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
            div class="table-wrap" {
                table {
                    thead {
                        tr { th { "Run" } th { "Tier" } th { "Received" } th { "Person" } }
                    }
                    tbody {
                        @for view in views {
                            tr {
                                td { a href=(format!("/runs/{}", view.run_id)) { (view.run_id) } }
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
            problems: judged.problems.join("; "),
        });
    }
    Ok(rows)
}

fn trust_name(trust: Option<Trust>) -> &'static str {
    match trust {
        Some(Trust::SelfDeclared) => "self_declared",
        Some(Trust::AgentClaimed) => "agent_claimed",
        Some(Trust::EvidenceVerified) => "evidence_verified",
        None => "none",
    }
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
                    "body { margin: 0; padding: 0.5rem; font-family: system-ui, sans-serif; } h1, h2 { font-size: 1.1rem; } .table-wrap { overflow-x: auto; } table { border-collapse: collapse; width: 100%; } th, td { text-align: left; padding: 0.25rem; vertical-align: top; } code, pre { overflow-wrap: anywhere; white-space: pre-wrap; }"
                }
            }
            body { (content) }
        }
    }
}
