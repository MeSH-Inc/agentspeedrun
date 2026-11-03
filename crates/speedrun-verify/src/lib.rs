//! Evidence verifiers. A milestone reaches `evidence_verified` only when the
//! verifier itself has checked every piece of verifiable evidence attached,
//! and the evidence covers what the milestone's category key asks for.
//!
//! One library for both judges: the local daemon's preview and the commons
//! server's judgment (COMMONS.md §6). If the two drifted, every published
//! number would be arguable. A job without a project root, as on the server,
//! cannot check git commits or match a CI repository against git remotes.
//!
//! Network checks (URLs, GitHub Actions, Stripe) run off the store lock;
//! see `Service::finish_verification`. Nothing fetched here is stored: the
//! log keeps only the evidence references the client sent.

use std::collections::BTreeSet;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

pub mod git;

use crate::git::{commit_time as git_commit_time, output as git_output};
use chrono::{DateTime, Utc};
use serde_json::Value;
use speedrun_core::category::VerificationPolicy;
use speedrun_core::{
    Evidence,
    api::{VerificationFact, VerificationReport, VerificationScope, VerificationStatus},
    paths,
};

/// At most this many network-checked items per claim.
const MAX_REMOTE_EVIDENCE: usize = 6;
/// Bodies beyond this are truncated before inspection.
const MAX_BODY: u64 = 2 * 1024 * 1024;
/// A legal page needs at least this much server-rendered text.
const MIN_LEGAL_WORDS: usize = 200;

/// Everything a verification needs, captured under the store lock.
#[derive(Debug, Clone)]
pub struct Job {
    pub scope: VerificationScope,
    pub run_id: String,
    pub cause_event_id: String,
    pub milestone_id: String,
    /// Category key, for predefined milestones.
    pub key: Option<String>,
    /// Explicit ruleset policy; absent for historical category versions.
    pub policy: Option<VerificationPolicy>,
    /// Set only after the daemon validates the single referenced observation.
    pub local_test_verified: bool,
    pub evidence: Vec<Evidence>,
    pub project_root: Option<String>,
    pub run_started_at: DateTime<Utc>,
    pub claimed_at: DateTime<Utc>,
    /// The URL the run's `deployed` milestone was verified with, if it was.
    /// Legal pages must be on its registrable domain (A3-4).
    pub deployed_url: Option<String>,
}

impl Job {
    /// Worth running: something besides notes is attached.
    pub fn has_checkable_evidence(evidence: &[Evidence]) -> bool {
        evidence.iter().any(|e| !matches!(e, Evidence::Note { .. }))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub verifiers: Vec<&'static str>,
    pub problems: Vec<String>,
    pub scope: VerificationScope,
    pub unavailable: bool,
    pub facts: Vec<VerificationFact>,
}

impl Outcome {
    pub fn verified(&self) -> bool {
        self.problems.is_empty() && !self.verifiers.is_empty()
    }

    pub fn report(&self) -> VerificationReport {
        VerificationReport {
            verified: self.verified(),
            verifier: self.verified().then(|| self.verifiers.join("+")),
            problems: self.problems.clone(),
            scope: self.scope,
            status: if self.verified() {
                VerificationStatus::Verified
            } else if self.unavailable {
                VerificationStatus::Unavailable
            } else {
                VerificationStatus::Failed
            },
            facts: self.facts.clone(),
            observer: "speedrun-verify".into(),
            observer_version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// A fetched resource. Error statuses are returned, not raised, so API
/// error messages can be shown.
pub struct Page {
    pub status: u16,
    /// After redirects.
    pub url: String,
    pub body: String,
}

/// HTTPS access, behind a trait so tests can run offline.
pub trait Fetch: Send + Sync {
    /// GET `url`. API calls (`api = true`) send `headers` and never follow
    /// redirects; page fetches follow up to five, HTTPS only.
    fn get(&self, url: &str, headers: &[(&str, &str)], api: bool) -> Result<Page, String>;
}

/// Credentials for the API verifiers, looked up at verification time so
/// `speedrun secret set` needs no daemon restart.
pub trait Secrets: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
}

/// `$SPEEDRUN_HOME/secrets/<name>`, overridable by environment variables,
/// plus the GitHub CLI's token as a fallback for `github`.
pub struct DiskSecrets {
    pub dir: PathBuf,
}

impl Default for DiskSecrets {
    fn default() -> Self {
        Self {
            dir: paths::secrets_dir(),
        }
    }
}

impl Secrets for DiskSecrets {
    fn get(&self, name: &str) -> Option<String> {
        let env: &[&str] = match name {
            "stripe" => &["SPEEDRUN_STRIPE_KEY"],
            "github" => &["SPEEDRUN_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"],
            _ => &[],
        };
        let nonempty = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        env.iter()
            .find_map(|v| std::env::var(v).ok().and_then(nonempty))
            .or_else(|| {
                std::fs::read_to_string(self.dir.join(name))
                    .ok()
                    .and_then(nonempty)
            })
            .or_else(|| {
                (name == "github")
                    .then(|| Command::new("gh").args(["auth", "token"]).output().ok())
                    .flatten()
                    .filter(|o| o.status.success())
                    .and_then(|o| nonempty(String::from_utf8_lossy(&o.stdout).into_owned()))
            })
    }
}

/// No secrets at all: GitHub is read anonymously and Stripe cannot be checked.
/// A verifier that judges other people's claims (the commons) uses this, so
/// its operator's own accounts never decide them.
pub struct NoSecrets;

impl Secrets for NoSecrets {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}

pub struct Verifier {
    pub fetch: Box<dyn Fetch>,
    pub secrets: Box<dyn Secrets>,
}

impl Default for Verifier {
    fn default() -> Self {
        Self {
            fetch: Box::new(Net::new()),
            secrets: Box::new(DiskSecrets::default()),
        }
    }
}

/// What a predefined milestone's evidence must include, by category key.
fn required_kind(key: Option<&str>) -> Option<(&'static str, &'static str)> {
    Some(match key? {
        "deployed" => ("url", "the production URL (--url)"),
        "legal-pages" => ("url", "the terms and privacy policy URLs (--url, once each)"),
        "tests-green" => ("ci_run", "a GitHub Actions run URL (--ci)"),
        "first-payment" => ("stripe_event", "a Stripe payment, charge or event id (--stripe)"),
        _ => return None,
    })
}

fn kind(ev: &Evidence) -> &'static str {
    match ev {
        Evidence::GitCommit { .. } => "git_commit",
        Evidence::Url { .. } => "url",
        Evidence::CiRun { .. } => "ci_run",
        Evidence::LocalTest { .. } => "local_test",
        Evidence::StripeEvent { .. } => "stripe_event",
        Evidence::Note { .. } => "note",
    }
}

impl Verifier {
    /// Check every piece of evidence. `note` evidence is context, not proof.
    pub fn verify(&self, job: &Job) -> Outcome {
        let mut out = Outcome {
            scope: job.scope,
            ..Outcome::default()
        };
        let legacy_key = job.key.as_deref();
        let policy_key = match job.policy {
            Some(VerificationPolicy::SemanticClaim) => {
                out.problems.push(
                    "this milestone requires a semantic claim; attached evidence alone cannot verify it"
                        .into(),
                );
                return out;
            }
            Some(VerificationPolicy::Deployment) => Some("deployed"),
            Some(VerificationPolicy::LegalPages) => Some("legal-pages"),
            Some(VerificationPolicy::LocalTestsOrCi) => Some("tests-green"),
            Some(VerificationPolicy::FirstPayment) => Some("first-payment"),
            None => legacy_key,
        };
        let local_count = job
            .evidence
            .iter()
            .filter(|e| matches!(e, Evidence::LocalTest { .. }))
            .count();
        if local_count > 1 {
            out.problems
                .push("at most one local test observation may be attached".into());
        }
        let local_or_ci = job.policy == Some(VerificationPolicy::LocalTestsOrCi);
        if local_or_ci
            && !job
                .evidence
                .iter()
                .any(|e| matches!(e, Evidence::LocalTest { .. } | Evidence::CiRun { .. }))
        {
            out.problems
                .push("this milestone requires a local test observation or CI run".into());
        }
        if !local_or_ci
            && let Some((needed, what)) = required_kind(policy_key)
            && !job.evidence.iter().any(|e| kind(e) == needed)
        {
            out.problems.push(format!("this milestone is verified by {what}"));
        }
        let remote = job
            .evidence
            .iter()
            .filter(|e| {
                !matches!(
                    e,
                    Evidence::Note { .. } | Evidence::GitCommit { .. } | Evidence::LocalTest { .. }
                )
            })
            .count();
        if remote > MAX_REMOTE_EVIDENCE {
            out.problems.push(format!(
                "at most {MAX_REMOTE_EVIDENCE} URLs, CI runs and payments per claim"
            ));
            return out;
        }

        let key = policy_key;
        let deployed = job.deployed_url.as_deref();
        let mut legal = LegalCoverage::default();
        for ev in &job.evidence {
            let unavailable = match ev {
                Evidence::GitCommit { .. } | Evidence::CiRun { .. } => {
                    job.scope == VerificationScope::Public || job.project_root.is_none()
                }
                Evidence::LocalTest { .. } => job.scope == VerificationScope::Public,
                Evidence::StripeEvent { .. } => {
                    let key = self.secrets.get("stripe");
                    job.scope == VerificationScope::Public
                        || key.is_none()
                        || (job.project_root.is_none()
                            && key
                                .as_deref()
                                .is_some_and(|k| k.starts_with("rk_live_") || k.starts_with("sk_live_")))
                }
                _ => false,
            };
            if unavailable {
                out.unavailable = true;
                out.problems
                    .push(format!("{} verification is unavailable in this scope", kind(ev)));
                continue;
            }
            let (name, result) = match ev {
                Evidence::Note { .. } => continue,
                Evidence::GitCommit { sha } => (
                    "git",
                    match job.project_root.as_deref() {
                        Some(root) => git_commit_evidence(root, sha, job.run_started_at.timestamp()),
                        None => Err("the run has no project root to check commits against".into()),
                    },
                ),
                Evidence::Url { url } => ("url", self.check_url(url, key, deployed, &mut legal)),
                Evidence::CiRun { url } => ("ci", self.check_ci(url, job)),
                Evidence::LocalTest { .. } => (
                    "local_test",
                    if local_or_ci && local_count == 1 && job.local_test_verified {
                        Ok(())
                    } else {
                        Err("local test observation could not be verified".into())
                    },
                ),
                Evidence::StripeEvent { event_id } => ("stripe", self.check_stripe(event_id, job)),
            };
            match result {
                Ok(()) => {
                    out.verifiers.push(name);
                    let fact = match ev {
                        Evidence::GitCommit { .. } => Some(VerificationFact::GitCommitInHistory),
                        Evidence::Url { .. } if policy_key != Some("legal-pages") => {
                            Some(VerificationFact::PublicUrlReachable)
                        }
                        Evidence::CiRun { .. } => Some(VerificationFact::CiRunPassed),
                        Evidence::LocalTest { .. } => Some(VerificationFact::LocalTestsPassed),
                        Evidence::StripeEvent { .. } => Some(VerificationFact::StripePaymentValid),
                        _ => None,
                    };
                    if let Some(fact) = fact {
                        out.facts.push(fact);
                    }
                }
                Err(e) => out.problems.push(e),
            }
        }
        // Coverage only means something once every page loaded.
        if policy_key == Some("legal-pages") && out.problems.is_empty() {
            out.problems.extend(legal.missing());
            if out.problems.is_empty() {
                out.facts.push(VerificationFact::LegalPagesPresent);
            }
        }
        out.verifiers.sort_unstable();
        out.verifiers.dedup();
        out
    }

    // --- URLs ------------------------------------------------------------

    fn check_url(
        &self,
        url: &str,
        key: Option<&str>,
        deployed_url: Option<&str>,
        legal: &mut LegalCoverage,
    ) -> Result<(), String> {
        public_https_url(url)?;
        let legal_page = key == Some("legal-pages");
        if legal_page {
            legal_domain(url, deployed_url)?;
        }
        let page = self.fetch.get(url, &[], false)?;
        public_https_url(&page.url).map_err(|e| format!("{url} redirects to {}: {e}", page.url))?;
        if legal_page {
            legal_domain(&page.url, deployed_url)?;
        }
        if !(200..300).contains(&page.status) {
            return Err(format!("{url} answered HTTP {}", page.status));
        }
        let raw = if legal_page {
            legal_text(&page.body)
        } else {
            visible_text(&page.body)
        };
        let text = raw.to_lowercase();
        let short = text.split_whitespace().count() < PLACEHOLDER_MAX_WORDS;
        let sign = PLACEHOLDER_SIGNS.iter().find(|s| short && text.contains(*s));
        if let Some(sign) = sign.or(["lorem ipsum"].iter().find(|s| text.contains(*s))) {
            return Err(format!("{url} looks like a placeholder page (\"{sign}\")"));
        }
        if legal_page {
            legal.add(url, &text)?;
        }
        Ok(())
    }

    // --- GitHub Actions ----------------------------------------------------

    fn check_ci(&self, url: &str, job: &Job) -> Result<(), String> {
        let (owner, repo, id) = parse_actions_run(url)?;
        let root = job
            .project_root
            .as_deref()
            .ok_or("the run has no project root to match the CI repository against")?;
        if !git_remote_matches(root, &owner, &repo) {
            return Err(format!("{owner}/{repo} is not a git remote of this project"));
        }
        let token = self.secrets.get("github");
        let auth = token.as_ref().map(|t| format!("Bearer {t}"));
        let mut headers = vec![
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ];
        if let Some(a) = &auth {
            headers.push(("Authorization", a));
        }
        let api = format!("https://api.github.com/repos/{owner}/{repo}/actions/runs/{id}");
        let page = self.fetch.get(&api, &headers, true)?;
        let run: Value = match page.status {
            200 => {
                serde_json::from_str(&page.body).map_err(|e| format!("GitHub sent unreadable JSON: {e}"))?
            }
            404 if token.is_none() => {
                return Err(format!(
                    "GitHub run {id} not found; for a private repo run `speedrun secret set github` or `gh auth login`"
                ));
            }
            s => {
                return Err(format!(
                    "GitHub API answered HTTP {s} for run {id}: {}",
                    api_message(&page.body)
                ));
            }
        };
        let what = format!("CI run {id}");
        let field = |k: &str| required_str(&run, k, &what);
        let status = field("status")?;
        if status != "completed" {
            return Err(format!("CI run {id} has not finished (status {status})"));
        }
        let conclusion = field("conclusion")?;
        if conclusion != "success" {
            return Err(format!("CI run {id} concluded {conclusion}"));
        }
        let sha = field("head_sha")?;
        if !git_commit_in_history(root, sha) {
            return Err(format!(
                "CI run {id} tested {sha}, which is not in this project's history"
            ));
        }
        if commit_predates_run(root, sha, job.run_started_at.timestamp())? {
            return Err(format!(
                "CI run {id} tested commit {sha}, which predates this speedrun"
            ));
        }
        let created = DateTime::parse_from_rfc3339(field("created_at")?)
            .map_err(|_| format!("CI run {id} has no readable created_at"))?;
        if created < job.run_started_at {
            return Err(format!("CI run {id} ran before this speedrun started"));
        }
        Ok(())
    }

    // --- Stripe -------------------------------------------------------------

    fn check_stripe(&self, id: &str, job: &Job) -> Result<(), String> {
        let key = self.secrets.get("stripe").ok_or(
            "no Stripe key; run `speedrun secret set stripe` with a restricted live-mode key (rk_live_…)",
        )?;
        if !(key.starts_with("rk_live_") || key.starts_with("sk_live_")) {
            return Err("the stored Stripe key is not a live-mode key, so it cannot see real revenue".into());
        }
        let stripe = Stripe {
            fetch: &*self.fetch,
            auth: format!("Bearer {key}"),
            since: job.run_started_at.timestamp(),
            // Without a project there are no committers to tell apart from customers.
            founders: match job.project_root.as_deref() {
                Some(root) => founder_emails(root),
                None => BTreeSet::new(),
            },
        };
        stripe.check(id, 0)
    }
}

// --- URL helpers ------------------------------------------------------------

/// Short pages with these prove nothing: server defaults, parked domains, stubs.
const PLACEHOLDER_SIGNS: &[&str] = &[
    "welcome to nginx",
    "apache2 default page",
    "it works!",
    "this domain is for sale",
    "domain is parked",
    "coming soon",
    "under construction",
];
/// Pages longer than this are real content even if they mention a sign.
const PLACEHOLDER_MAX_WORDS: usize = 150;

/// Tunnels expose a developer's machine; they are not a deployment.
const TUNNEL_SUFFIXES: &[&str] = &[
    "ngrok.io",
    "ngrok.app",
    "ngrok-free.app",
    "ngrok-free.dev",
    "trycloudflare.com",
    "loca.lt",
    "localtunnel.me",
    "serveo.net",
    "localhost.run",
    "lhr.life",
    "pinggy.io",
    "pinggy.link",
    "devtunnels.ms",
];

const PRIVATE_SUFFIXES: &[&str] = &[
    "localhost",
    "local",
    "internal",
    "lan",
    "home",
    "home.arpa",
    "test",
    "example",
    "invalid",
    "corp",
];

/// An `https://` URL on a public DNS name: no IP literals, local names or tunnels.
pub fn public_https_url(raw: &str) -> Result<(), String> {
    let url = url::Url::parse(raw).map_err(|e| format!("{raw} is not a URL: {e}"))?;
    if url.scheme() != "https" {
        return Err(format!("{raw} is not HTTPS"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{raw} embeds credentials"));
    }
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').to_ascii_lowercase(),
        Some(_) => return Err(format!("{raw} uses an IP address, not a domain")),
        None => return Err(format!("{raw} has no host")),
    };
    let under = |suffix: &str| host == suffix || host.ends_with(&format!(".{suffix}"));
    if !host.contains('.') || PRIVATE_SUFFIXES.iter().any(|s| under(s)) {
        return Err(format!("{host} is not a public domain"));
    }
    if TUNNEL_SUFFIXES.iter().any(|s| under(s)) {
        return Err(format!("{host} is a tunnel to a local machine, not a deployment"));
    }
    Ok(())
}

/// Text a reader sees without running JavaScript.
pub fn visible_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    while i < html.len() {
        if lower[i..].starts_with('<') {
            let skip_to = ["script", "style", "noscript", "template"]
                .iter()
                .find(|t| lower[i + 1..].starts_with(*t))
                .and_then(|t| lower[i..].find(&format!("</{t}")).map(|end| i + end));
            let from = skip_to.unwrap_or(i);
            i = lower[from..].find('>').map_or(html.len(), |end| from + end + 1);
            out.push(' ');
        } else {
            let next = lower[i..].find('<').map_or(html.len(), |n| i + n);
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&rsquo;", "'")
        .replace("&quot;", "\"");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Tags whose text is not legal-page content: scripts and styles are never
/// rendered, and links, navigation and footers are a page's chrome rather
/// than the document it carries.
const NON_CONTENT_TAGS: &[&str] = &["a", "nav", "footer", "script", "style", "noscript", "template"];

/// The tag name that opens at `at`, if it is one of [`NON_CONTENT_TAGS`]. A
/// name matches only at a boundary, so `<a>` is dropped but `<article>` is not.
fn opens_non_content_tag(lower: &str, at: usize) -> Option<&'static str> {
    NON_CONTENT_TAGS.iter().copied().find(|tag| {
        let rest = match lower.get(at + 1..) {
            Some(rest) => rest,
            None => return false,
        };
        let after = match rest.strip_prefix(*tag) {
            Some(after) => after,
            None => return false,
        };
        match after.chars().next() {
            None => true,
            Some(c) => c == '>' || c == '/' || c.is_ascii_whitespace(),
        }
    })
}

/// Where `</name` starts at or after `from`, matched at a name boundary so
/// the closer of `<abbr>` is not mistaken for `</a>`.
fn closing_tag(lower: &str, from: usize, name: &str) -> Option<usize> {
    let needle = format!("</{name}");
    let mut at = from;
    loop {
        let hit = at + lower[at..].find(&needle)?;
        let after = hit + needle.len();
        let boundary = match lower.get(after..).and_then(|rest| rest.chars().next()) {
            None => true,
            Some(c) => c == '>' || c == '/' || c.is_ascii_whitespace(),
        };
        if boundary {
            return Some(hit);
        }
        at = after;
    }
}

/// Just past the closing tag of the non-content element opening at `at`, or
/// `None` when the markup there is ordinary content or never closes.
fn skip_element(lower: &str, at: usize) -> Option<usize> {
    let tag = opens_non_content_tag(lower, at)?;
    let hit = closing_tag(lower, at, tag)?;
    let end = match lower[hit..].find('>') {
        Some(end) => hit + end + 1,
        None => lower.len(),
    };
    Some(end)
}

/// Text a reader sees on a legal page: like [`visible_text`], but the text
/// inside links, navigation and footers is dropped, so a "Privacy Policy"
/// link in a site's chrome does not make the page around it a policy.
fn legal_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    while i < html.len() {
        if lower[i..].starts_with('<') {
            if let Some(end) = skip_element(&lower, i) {
                i = end;
            } else {
                i = match lower[i..].find('>') {
                    Some(end) => i + end + 1,
                    None => html.len(),
                };
            }
            out.push(' ');
        } else {
            let next = match lower[i..].find('<') {
                Some(next) => i + next,
                None => html.len(),
            };
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&rsquo;", "'")
        .replace("&quot;", "\"");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The last two host labels of `raw`: the approximation of the registrable
/// domain the legal-pages rule compares.
fn registrable_domain(raw: &str) -> Result<String, String> {
    let url = url::Url::parse(raw).map_err(|e| format!("{raw} is not a URL: {e}"))?;
    let host = match url.host_str() {
        Some(host) => host.trim_end_matches('.').to_ascii_lowercase(),
        None => return Err(format!("{raw} has no host")),
    };
    let labels: Vec<&str> = host.split('.').collect();
    let skip = labels.len().saturating_sub(2);
    Ok(labels.into_iter().skip(skip).collect::<Vec<_>>().join("."))
}

/// Legal pages must sit on the deployment's own site, and only once the run
/// has a verified deployment URL to compare against.
fn legal_domain(url: &str, deployed_url: Option<&str>) -> Result<(), String> {
    let deployed = match deployed_url {
        Some(deployed) => deployed,
        None => return Err("legal pages need a verified deployment to compare against".into()),
    };
    let page = registrable_domain(url)?;
    let site = registrable_domain(deployed)?;
    if page != site {
        return Err(format!("{url} is on {page}, not the deployment's {site}"));
    }
    Ok(())
}

/// Which legal documents the fetched pages provide, with the URL that
/// provided each, so one page cannot stand in for both.
#[derive(Default)]
struct LegalCoverage {
    terms_urls: Vec<String>,
    privacy_urls: Vec<String>,
}

impl LegalCoverage {
    fn add(&mut self, url: &str, text: &str) -> Result<(), String> {
        let terms = [
            "terms of service",
            "terms of use",
            "terms and conditions",
            "terms & conditions",
        ]
        .iter()
        .any(|p| text.contains(p));
        let privacy = ["privacy policy", "privacy notice", "privacy statement"]
            .iter()
            .any(|p| text.contains(p));
        if !terms && !privacy {
            return Err(format!("{url} is neither terms of service nor a privacy policy"));
        }
        let words = text.split_whitespace().count();
        if words < MIN_LEGAL_WORDS {
            return Err(format!(
                "{url} has only {words} words of server-rendered text; legal pages must be readable without JavaScript"
            ));
        }
        if terms {
            self.terms_urls.push(url.to_string());
        }
        if privacy {
            self.privacy_urls.push(url.to_string());
        }
        Ok(())
    }

    fn missing(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.terms_urls.is_empty() {
            out.push("no terms of service page among the URLs".into());
        }
        if self.privacy_urls.is_empty() {
            out.push("no privacy policy page among the URLs".into());
        }
        let distinct = self
            .terms_urls
            .iter()
            .any(|terms| self.privacy_urls.iter().any(|privacy| privacy != terms));
        if !self.terms_urls.is_empty() && !self.privacy_urls.is_empty() && !distinct {
            out.push("the terms and privacy policy must be different URLs".into());
        }
        out
    }
}

fn api_message(body: &str) -> String {
    let message = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    // An error body need not carry a message; this only words an error already found.
    match message {
        Some(message) => message,
        None => "no details".into(),
    }
}

/// A string field the API always sends. Its absence means the response is not
/// the object that was asked for, so the evidence does not verify.
fn required_str<'a>(object: &'a Value, key: &str, what: &str) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{what} has no {key}"))
}

/// An integer field the API always sends; see [`required_str`].
fn required_i64(object: &Value, key: &str, what: &str) -> Result<i64, String> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{what} has no {key}"))
}

/// A boolean field the API always sends; see [`required_str`].
fn required_bool(object: &Value, key: &str, what: &str) -> Result<bool, String> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{what} has no {key}"))
}

// --- GitHub helpers ---------------------------------------------------------

/// `https://github.com/<owner>/<repo>/actions/runs/<id>[/...]`.
fn parse_actions_run(raw: &str) -> Result<(String, String, u64), String> {
    let bad = || {
        format!("{raw} is not a GitHub Actions run URL (https://github.com/<owner>/<repo>/actions/runs/<id>)")
    };
    let url = url::Url::parse(raw).map_err(|_| bad())?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        return Err(bad());
    }
    let seg: Vec<&str> = url.path_segments().ok_or_else(bad)?.collect();
    let name_ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            && s != ".."
    };
    match seg.as_slice() {
        [owner, repo, "actions", "runs", id, ..] if name_ok(owner) && name_ok(repo) => Ok((
            owner.to_string(),
            repo.to_string(),
            id.parse().map_err(|_| bad())?,
        )),
        _ => Err(bad()),
    }
}

/// Some remote of the project points at github.com/<owner>/<repo>.
fn git_remote_matches(root: &str, owner: &str, repo: &str) -> bool {
    let want = format!("{owner}/{repo}").to_ascii_lowercase();
    git_output(root, &["remote", "-v"]).is_some_and(|out| {
        out.split_whitespace().any(|u| {
            let u = u.to_ascii_lowercase();
            let path = u
                .strip_prefix("https://github.com/")
                .or_else(|| u.strip_prefix("git@github.com:"))
                .or_else(|| u.strip_prefix("ssh://git@github.com/"));
            path.is_some_and(|p| p.trim_end_matches('/').trim_end_matches(".git") == want)
        })
    })
}

/// The commit exists and is an ancestor of (or equal to) HEAD.
fn git_commit_in_history(root: &str, sha: &str) -> bool {
    let well_formed = (7..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
    well_formed
        && git_output(root, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_some()
        && git_output(root, &["merge-base", "--is-ancestor", sha, "HEAD"]).is_some()
}

/// Whether `sha` was committed before the run began. Git records whole
/// seconds, so a commit made in the same second still counts.
fn commit_predates_run(root: &str, sha: &str, run_started_secs: i64) -> Result<bool, String> {
    let committed = git_commit_time(root, sha)?;
    Ok(committed < run_started_secs)
}

/// A commit verifies only if it is in this project's history and was made no
/// earlier than the run began.
fn git_commit_evidence(root: &str, sha: &str, run_started_secs: i64) -> Result<(), String> {
    if !git_commit_in_history(root, sha) {
        return Err(format!("commit {sha} is not in this project's history"));
    }
    if commit_predates_run(root, sha, run_started_secs)? {
        return Err(format!("commit {sha} was made before this speedrun started"));
    }
    Ok(())
}

/// Everyone who has committed to the project: payments from them are not revenue.
fn founder_emails(root: &str) -> BTreeSet<String> {
    // A project with no commits yet has no log, and one with no user.email has
    // no configured committer; either way that source lists nobody.
    let sources = [
        git_output(root, &["log", "--format=%ae%n%ce"]),
        git_output(root, &["config", "user.email"]),
    ];
    let mut out: BTreeSet<String> = sources
        .iter()
        .flatten()
        .flat_map(|text| text.lines())
        .map(normalize_email)
        .filter(|e| e.contains('@'))
        .collect();
    out.remove("noreply@github.com");
    out
}

/// How a payment email is compared to a committer's: case, a `+tag` on the
/// local part, and dots in Gmail addresses do not make a different person.
/// This is the single definition of email identity for Stripe checks.
fn normalize_email(raw: &str) -> String {
    let email = raw.trim().to_ascii_lowercase();
    match email.rsplit_once('@') {
        Some((local, domain)) => {
            let local = match local.split_once('+') {
                Some((local, _tag)) => local,
                None => local,
            };
            let local = if domain == "gmail.com" || domain == "googlemail.com" {
                local.replace('.', "")
            } else {
                local.to_string()
            };
            format!("{local}@{domain}")
        }
        None => email,
    }
}

// --- Stripe helpers ---------------------------------------------------------

struct Stripe<'a> {
    fetch: &'a dyn Fetch,
    auth: String,
    /// Payments must be made after the run started (unix seconds).
    since: i64,
    founders: BTreeSet<String>,
}

/// The Stripe API version every request pins, so a response's shape cannot
/// change under a verifier that has already been reviewed. At this version an
/// invoice still names its `charge` and `payment_intent`; the
/// `payments.data[].payment` fallback only matters if the pin moves to
/// 2025-03-31.basil or later, which drops those fields. Moving the pin is a
/// review of every Stripe path, not a constant edit.
const STRIPE_API_VERSION: &str = "2024-06-20";

/// A refund above this percentage of the captured amount counts as refunded;
/// smaller partial refunds still leave real revenue.
const REFUND_TOLERANCE_PERCENT: i64 = 10;

const STRIPE_PAYMENT_EVENTS: &[&str] = &[
    "charge.succeeded",
    "payment_intent.succeeded",
    "checkout.session.completed",
    "invoice.paid",
    "invoice.payment_succeeded",
];

impl Stripe<'_> {
    fn object(&self, path: &str) -> Result<Value, String> {
        let url = format!("https://api.stripe.com/v1/{path}");
        let page = self.fetch.get(
            &url,
            &[
                ("Authorization", self.auth.as_str()),
                ("Stripe-Version", STRIPE_API_VERSION),
            ],
            true,
        )?;
        match page.status {
            200 => serde_json::from_str(&page.body).map_err(|e| format!("Stripe sent unreadable JSON: {e}")),
            401 => Err(format!(
                "Stripe rejected the stored key ({}); store a valid one with `speedrun secret set stripe`",
                api_message(&page.body)
            )),
            403 => Err(format!(
                "the Stripe key cannot read {path}: {} (give the restricted key read access to it)",
                api_message(&page.body)
            )),
            s => Err(format!(
                "Stripe answered HTTP {s} for {path}: {}",
                api_message(&page.body)
            )),
        }
    }

    /// `id` is an event, charge, PaymentIntent, Checkout Session or invoice.
    fn check(&self, id: &str, depth: u8) -> Result<(), String> {
        let well_formed = id.len() <= 255 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !well_formed || depth > 3 {
            return Err(format!("{id} is not a Stripe id"));
        }
        let prefix = match id.split_once('_') {
            Some((prefix, _)) => prefix,
            None => id,
        };
        let path = match prefix {
            "evt" => format!("events/{id}"),
            "ch" | "py" => format!("charges/{id}"),
            "pi" => format!("payment_intents/{id}"),
            "cs" => format!("checkout/sessions/{id}"),
            "in" => format!("invoices/{id}"),
            _ => {
                return Err(format!(
                    "{id} is not a Stripe event, charge, payment, checkout session or invoice id"
                ));
            }
        };
        let obj = self.object(&path)?;
        if obj.get("livemode").and_then(Value::as_bool) != Some(true) {
            return Err(format!("{id} is a test-mode object, not real revenue"));
        }
        let what = format!("Stripe object {id}");
        let s = |k: &str| required_str(&obj, k, &what);
        let n = |k: &str| required_i64(&obj, k, &what);
        let linked = |k: &str| {
            obj.get(k)
                .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(Value::as_str)))
                .map(str::to_string)
        };
        match prefix {
            "evt" => {
                let kind = s("type")?;
                if !STRIPE_PAYMENT_EVENTS.contains(&kind) {
                    return Err(format!("{id} is a {kind} event, not a payment"));
                }
                let inner = obj
                    .pointer("/data/object/id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{id} has no payment object"))?;
                // Re-read the object itself: it may have been refunded since.
                self.check(inner, depth + 1)
            }
            "ch" | "py" => self.check_charge(id, &obj),
            "pi" => {
                let status = s("status")?;
                if status != "succeeded" {
                    return Err(format!("payment {id} is {status}"));
                }
                let charge = linked("latest_charge")
                    .or_else(|| {
                        obj.pointer("/charges/data/0/id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .ok_or_else(|| format!("payment {id} has no charge"))?;
                self.check(&charge, depth + 1)
            }
            "cs" => {
                if s("payment_status")? != "paid" || n("amount_total")? <= 0 {
                    return Err(format!("checkout session {id} is not paid"));
                }
                let email = obj.pointer("/customer_details/email").and_then(Value::as_str);
                self.not_founder(id, email)?;
                self.after_start(id, n("created")?)?;
                let payment = linked("payment_intent")
                    .or_else(|| linked("invoice"))
                    .ok_or_else(|| format!("checkout session {id} has no payment to check"))?;
                self.check(&payment, depth + 1)
            }
            "in" => {
                if s("status")? != "paid" || n("amount_paid")? <= 0 {
                    return Err(format!("invoice {id} is not paid"));
                }
                if obj.get("paid_out_of_band").and_then(Value::as_bool) == Some(true) {
                    return Err(format!("invoice {id} was paid out of band, not by a customer"));
                }
                let email = obj.get("customer_email").and_then(Value::as_str);
                self.not_founder(id, email)?;
                // An invoice marked paid without a transition time was paid when it was created.
                let paid_at = match obj.pointer("/status_transitions/paid_at").and_then(Value::as_i64) {
                    Some(paid_at) => paid_at,
                    None => n("created")?,
                };
                self.after_start(id, paid_at)?;
                let payment =
                    invoice_payment(&obj).ok_or_else(|| format!("invoice {id} has no payment to check"))?;
                self.check(&payment, depth + 1)
            }
            _ => Err(format!("{id} is not a Stripe id")),
        }
    }

    fn check_charge(&self, id: &str, ch: &Value) -> Result<(), String> {
        let what = format!("charge {id}");
        let b = |k: &str| required_bool(ch, k, &what);
        let n = |k: &str| required_i64(ch, k, &what);
        if !b("paid")? || ch.get("status").and_then(Value::as_str) != Some("succeeded") || !b("captured")? {
            return Err(format!("charge {id} did not succeed"));
        }
        let captured = match n("amount_captured")? {
            captured if captured > 0 => captured,
            _ => n("amount")?,
        };
        if captured <= 0 {
            return Err(format!("charge {id} captured nothing"));
        }
        if b("refunded")? || n("amount_refunded")? * 100 > captured * REFUND_TOLERANCE_PERCENT {
            return Err(format!("charge {id} was refunded"));
        }
        if b("disputed")? {
            return Err(format!("charge {id} is disputed"));
        }
        self.after_start(id, n("created")?)?;
        let emails = [
            ch.pointer("/billing_details/email").and_then(Value::as_str),
            ch.get("receipt_email").and_then(Value::as_str),
        ];
        let mut has_email = false;
        for address in emails.into_iter().flatten() {
            has_email = true;
            self.not_founder(id, Some(address))?;
        }
        if !has_email {
            return Err(format!("charge {id} has no customer email"));
        }
        Ok(())
    }

    fn after_start(&self, id: &str, created: i64) -> Result<(), String> {
        if created < self.since {
            return Err(format!("{id} was paid before this speedrun started"));
        }
        Ok(())
    }

    fn not_founder(&self, id: &str, email: Option<&str>) -> Result<(), String> {
        match email.map(normalize_email) {
            Some(address) if self.founders.contains(&address) => Err(format!(
                "{id} was paid by a committer to this project, not a customer"
            )),
            _ => Ok(()),
        }
    }
}

/// A Stripe id is either the id string itself or an expanded object's `id`.
fn payment_object_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

/// The charge an invoice was ultimately paid by. Stripe versions expose this
/// in different places, so follow `charge`, then `payment_intent`, then the
/// invoice's own payment list. `None` means there is no payment to check, and
/// the caller must fail closed rather than count the invoice on its own.
fn invoice_payment(invoice: &Value) -> Option<String> {
    let direct = invoice
        .get("charge")
        .and_then(payment_object_id)
        .or_else(|| invoice.get("payment_intent").and_then(payment_object_id));
    direct.or_else(|| {
        let payments = invoice.pointer("/payments/data").and_then(Value::as_array)?;
        payments.iter().find_map(|entry| {
            let payment = entry.get("payment")?;
            payment
                .get("payment_intent")
                .and_then(payment_object_id)
                .or_else(|| payment.get("charge").and_then(payment_object_id))
        })
    })
}

// --- real network -----------------------------------------------------------

/// HTTPS client for verifiers. Refuses to connect to private addresses
/// (unless a proxy does the connecting) and reads at most `MAX_BODY`.
pub struct Net {
    pages: ureq::Agent,
    api: ureq::Agent,
    /// Where GitHub's API is, when it is not [`GITHUB_API`]: the base named by
    /// `SPEEDRUN_GITHUB_API`, and an agent that may reach it over plain HTTP
    /// on this machine. The variable is the operator's, as every other
    /// setting of the process is; a local judgment it sways is not one the
    /// commons accepts, because the commons judges again with its own.
    github: Option<(String, ureq::Agent)>,
}

/// The GitHub API every verifier request names.
const GITHUB_API: &str = "https://api.github.com";

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || a >= 224)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80)
            }
        },
    }
}

impl Net {
    /// The verifier's network, with GitHub's API where `SPEEDRUN_GITHUB_API`
    /// says, if it says.
    pub fn new() -> Self {
        let github_api = std::env::var("SPEEDRUN_GITHUB_API")
            .ok()
            .map(|base| base.trim().trim_end_matches('/').to_string())
            .filter(|base| !base.is_empty() && base != GITHUB_API);
        Self::with_github_api(github_api)
    }

    /// The verifier's network, with GitHub's API at `github_api` when given.
    pub fn with_github_api(github_api: Option<String>) -> Self {
        let proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
            .and_then(|p| ureq::Proxy::new(p).ok());
        let build = |redirects: u32| {
            let proxied = proxy.is_some();
            let mut b = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .https_only(true)
                .redirects(redirects)
                .user_agent(concat!(
                    "agentspeedrun-verifier/",
                    env!("CARGO_PKG_VERSION"),
                    " (+https://github.com/mesh-inc/agentspeedrun)"
                ))
                .resolver(move |netloc: &str| -> std::io::Result<Vec<SocketAddr>> {
                    let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
                    if proxied {
                        return Ok(addrs); // This is the proxy; it resolves the target.
                    }
                    let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| is_public(a.ip())).collect();
                    if public.is_empty() {
                        return Err(std::io::Error::other(format!("{netloc} has no public address")));
                    }
                    Ok(public)
                });
            if let Some(p) = proxy.clone() {
                b = b.proxy(p);
            }
            b.build()
        };
        let github = github_api.map(|base| {
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .redirects(0)
                .user_agent(concat!("agentspeedrun-verifier/", env!("CARGO_PKG_VERSION")))
                .build();
            (base, agent)
        });
        Self {
            pages: build(5),
            api: build(0),
            github,
        }
    }
}

impl Default for Net {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetch for Net {
    fn get(&self, url: &str, headers: &[(&str, &str)], api: bool) -> Result<Page, String> {
        let redirected = match (&self.github, url.strip_prefix(GITHUB_API)) {
            (Some((base, agent)), Some(rest)) if api => Some((format!("{base}{rest}"), agent)),
            _ => None,
        };
        let (url, agent) = match &redirected {
            Some((url, agent)) => (url.as_str(), *agent),
            None if api => (url, &self.api),
            None => (url, &self.pages),
        };
        let mut req = agent.get(url);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let res = match req.call() {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) if t.kind() == ureq::ErrorKind::InsecureRequestHttpsOnly => {
                return Err(format!("{url} redirects to a plain-HTTP page"));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(format!(
                    "could not fetch {url}: {}",
                    match t.message() {
                        Some(message) => message.to_string(),
                        None => t.kind().to_string(),
                    }
                ));
            }
        };
        let status = res.status();
        let final_url = res.get_url().to_string();
        let mut body = Vec::new();
        res.into_reader()
            .take(MAX_BODY)
            .read_to_end(&mut body)
            .map_err(|e| format!("reading {url}: {e}"))?;
        Ok(Page {
            status,
            url: final_url,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Canned responses by URL.
    #[derive(Default)]
    struct Fake(HashMap<String, (u16, String)>);

    impl Fake {
        fn with(mut self, url: &str, status: u16, body: impl Into<String>) -> Self {
            self.0.insert(url.into(), (status, body.into()));
            self
        }
    }

    impl Fetch for Fake {
        fn get(&self, url: &str, _headers: &[(&str, &str)], _api: bool) -> Result<Page, String> {
            let (status, body) = self.0.get(url).cloned().ok_or(format!("could not fetch {url}"))?;
            Ok(Page {
                status,
                url: url.into(),
                body,
            })
        }
    }

    struct Keys(&'static [(&'static str, &'static str)]);

    impl Secrets for Keys {
        fn get(&self, name: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn verifier(fake: Fake) -> Verifier {
        Verifier {
            fetch: Box::new(fake),
            secrets: Box::new(Keys(&[("stripe", "rk_live_x"), ("github", "ghp_x")])),
        }
    }

    fn job(key: Option<&str>, root: Option<&str>, evidence: Vec<Evidence>) -> Job {
        Job {
            scope: VerificationScope::Local,
            run_id: "r".into(),
            cause_event_id: "e".into(),
            milestone_id: key.unwrap_or("m").into(),
            key: key.map(str::to_string),
            policy: None,
            local_test_verified: false,
            evidence,
            project_root: root.map(str::to_string),
            run_started_at: DateTime::from_timestamp(1_000_000, 0).unwrap(),
            claimed_at: Utc::now(),
            deployed_url: Some("https://acme.dev".into()),
        }
    }

    fn url(u: &str) -> Evidence {
        Evidence::Url { url: u.into() }
    }

    fn stripe(id: &str) -> Evidence {
        Evidence::StripeEvent { event_id: id.into() }
    }

    /// A repo with one commit by founder@example.com and a GitHub remote.
    fn repo() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let git = |args: &[&str]| git_output(&root, args).expect("git");
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", "git@github.com:Acme/Shop.git"]);
        git(&[
            "-c",
            "user.name=f",
            "-c",
            "user.email=Founder@Example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        let sha = git(&["rev-parse", "HEAD"]).trim().to_string();
        (dir, root, sha)
    }

    #[test]
    fn rejects_non_hex_shas_without_running_git() {
        assert!(!git_commit_in_history("/", "--output=/tmp/x"));
        assert!(!git_commit_in_history("/", "abc"));
    }

    #[test]
    fn notes_alone_do_not_verify() {
        let out = verifier(Fake::default()).verify(&job(
            None,
            Some("/"),
            vec![Evidence::Note {
                text: "trust me".into(),
            }],
        ));
        assert!(!out.verified());
    }

    #[test]
    fn verifies_real_commit() {
        let (_d, root, sha) = repo();
        let v = verifier(Fake::default());
        let out = v.verify(&job(None, Some(&root), vec![Evidence::GitCommit { sha }]));
        assert_eq!(out.verifiers, vec!["git"]);
        assert!(out.verified());
        let bogus = Evidence::GitCommit {
            sha: "deadbeefdeadbeef".into(),
        };
        assert!(!v.verify(&job(None, Some(&root), vec![bogus])).verified());
    }

    #[test]
    fn only_public_https_domains_count() {
        assert!(public_https_url("https://shop.acme.dev/").is_ok());
        for bad in [
            "http://shop.acme.dev",
            "https://127.0.0.1",
            "https://[::1]/",
            "https://localhost:3000",
            "https://box.local",
            "https://intranet",
            "https://abc.ngrok-free.app",
            "https://x.trycloudflare.com",
            "https://user:pw@acme.dev",
            "not a url",
        ] {
            assert!(public_https_url(bad).is_err(), "{bad}");
        }
        assert!(!is_public("10.1.2.3".parse().unwrap()));
        assert!(!is_public("169.254.169.254".parse().unwrap()));
        assert!(!is_public("100.64.0.1".parse().unwrap()));
        assert!(!is_public("::ffff:192.168.0.1".parse().unwrap()));
        assert!(!is_public("fd00::1".parse().unwrap()));
        assert!(is_public("93.184.216.34".parse().unwrap()));
    }

    #[test]
    fn deployed_needs_a_live_real_page() {
        let v = verifier(
            Fake::default()
                .with("https://shop.acme.dev", 200, "<h1>Acme</h1><p>Buy things</p>")
                .with("https://down.acme.dev", 502, "bad gateway")
                .with("https://parked.acme.dev", 200, "<h1>Welcome to nginx!</h1>"),
        );
        assert!(
            v.verify(&job(Some("deployed"), None, vec![url("https://shop.acme.dev")]))
                .verified()
        );
        for (ev, why) in [
            (vec![], "verified by the production URL"),
            (vec![url("https://down.acme.dev")], "HTTP 502"),
            (vec![url("https://parked.acme.dev")], "placeholder"),
            (vec![url("https://localhost")], "not a public domain"),
        ] {
            let out = v.verify(&job(Some("deployed"), None, ev));
            assert!(!out.verified());
            assert!(out.problems.join("; ").contains(why), "{:?}", out.problems);
        }
    }

    #[test]
    fn legal_pages_need_both_documents_server_rendered() {
        let filler = "words ".repeat(MIN_LEGAL_WORDS);
        let v = verifier(
            Fake::default()
                .with(
                    "https://acme.dev/terms",
                    200,
                    format!("<h1>Terms of Service</h1><p>{filler}</p>"),
                )
                .with(
                    "https://acme.dev/privacy",
                    200,
                    format!("<h1>Privacy Policy</h1><p>{filler}</p>"),
                )
                .with(
                    "https://acme.dev/spa",
                    200,
                    "<div id=root></div><script>render('Privacy Policy')</script>",
                ),
        );
        let both = vec![url("https://acme.dev/terms"), url("https://acme.dev/privacy")];
        assert!(v.verify(&job(Some("legal-pages"), None, both)).verified());

        let one = v.verify(&job(
            Some("legal-pages"),
            None,
            vec![url("https://acme.dev/terms")],
        ));
        assert_eq!(one.problems, vec!["no privacy policy page among the URLs"]);

        let spa = v.verify(&job(
            Some("legal-pages"),
            None,
            vec![url("https://acme.dev/terms"), url("https://acme.dev/spa")],
        ));
        assert!(
            spa.problems.join(" ").contains("neither terms"),
            "{:?}",
            spa.problems
        );
    }

    #[test]
    fn visible_text_drops_markup_and_scripts() {
        let html = "<html><head><style>p{}</style><script>var a = '<b>';</script></head>\
                    <body><p>Terms &amp;  Conditions</p><noscript>enable js</noscript></body></html>";
        assert_eq!(visible_text(html), "Terms & Conditions");
    }

    fn ci_body(conclusion: &str, sha: &str, created: &str) -> String {
        serde_json::json!({
            "status": "completed", "conclusion": conclusion, "head_sha": sha, "created_at": created
        })
        .to_string()
    }

    #[test]
    fn the_github_api_variable_moves_api_requests_and_nothing_else() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let mut stream = stream;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
            line
        });
        let net = Net::with_github_api(Some(base));
        let page = net
            .get("https://api.github.com/repos/a/b/actions/runs/1", &[], true)
            .unwrap();
        assert_eq!((page.status, page.body.as_str()), (200, "{}"));
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("GET /repos/a/b/actions/runs/1 ")
        );
        // A page fetch is a user's URL, never GitHub's API: it keeps the public-only rule.
        let refused = match net.get("https://127.0.0.1:9/", &[], false) {
            Ok(page) => panic!("a private page was fetched: HTTP {}", page.status),
            Err(e) => e,
        };
        assert!(refused.contains("could not fetch"), "{refused}");
    }

    #[test]
    fn ci_runs_must_pass_on_this_projects_history() {
        let (_d, root, sha) = repo();
        let api = "https://api.github.com/repos/Acme/Shop/actions/runs";
        let v = verifier(
            Fake::default()
                .with(
                    &format!("{api}/1"),
                    200,
                    ci_body("success", &sha, "2030-01-01T00:00:00Z"),
                )
                .with(
                    &format!("{api}/2"),
                    200,
                    ci_body("failure", &sha, "2030-01-01T00:00:00Z"),
                )
                .with(
                    &format!("{api}/3"),
                    200,
                    ci_body("success", &"a".repeat(40), "2030-01-01T00:00:00Z"),
                )
                .with(
                    &format!("{api}/4"),
                    200,
                    ci_body("success", &sha, "1969-01-01T00:00:00Z"),
                ),
        );
        let ci = |u: &str| {
            v.verify(&job(
                Some("tests-green"),
                Some(&root),
                vec![Evidence::CiRun { url: u.into() }],
            ))
        };
        assert!(ci("https://github.com/Acme/Shop/actions/runs/1/job/9").verified());
        assert!(ci("https://github.com/Acme/Shop/actions/runs/2").problems[0].contains("failure"));
        assert!(
            ci("https://github.com/Acme/Shop/actions/runs/3").problems[0].contains("not in this project")
        );
        assert!(ci("https://github.com/Acme/Shop/actions/runs/4").problems[0].contains("before"));
        assert!(ci("https://github.com/Other/Repo/actions/runs/1").problems[0].contains("not a git remote"));
        assert!(
            ci("https://gitlab.com/Acme/Shop/-/pipelines/1").problems[0].contains("not a GitHub Actions")
        );
    }

    fn charge(id: &str, extra: Value) -> String {
        let mut c = serde_json::json!({
            "id": id, "object": "charge", "livemode": true, "paid": true, "status": "succeeded",
            "captured": true, "amount": 900, "amount_captured": 900, "amount_refunded": 0,
            "refunded": false, "disputed": false, "created": 2_000_000_000,
            "billing_details": {"email": "customer@buyer.com"}
        });
        c.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        c.to_string()
    }

    #[test]
    fn stripe_payments_must_be_real_live_customer_revenue() {
        use serde_json::json;
        let (_d, root, _) = repo();
        let api = "https://api.stripe.com/v1";
        let v = verifier(
            Fake::default()
                .with(&format!("{api}/charges/ch_ok"), 200, charge("ch_ok", json!({})))
                .with(&format!("{api}/charges/ch_test"), 200, charge("ch_test", json!({"livemode": false})))
                .with(&format!("{api}/charges/ch_ref"), 200, charge("ch_ref", json!({"refunded": true})))
                .with(&format!("{api}/charges/ch_old"), 200, charge("ch_old", json!({"created": 5})))
                .with(
                    &format!("{api}/charges/ch_self"),
                    200,
                    charge("ch_self", json!({"billing_details": {"email": "founder@example.com"}})),
                )
                .with(
                    &format!("{api}/events/evt_1"),
                    200,
                    json!({"livemode": true, "type": "charge.succeeded", "data": {"object": {"id": "ch_ref"}}})
                        .to_string(),
                )
                .with(
                    &format!("{api}/events/evt_2"),
                    200,
                    json!({"livemode": true, "type": "customer.created", "data": {"object": {"id": "cus_1"}}})
                        .to_string(),
                )
                .with(
                    &format!("{api}/payment_intents/pi_1"),
                    200,
                    json!({"livemode": true, "status": "succeeded", "latest_charge": "ch_ok"}).to_string(),
                )
                .with(
                    &format!("{api}/checkout/sessions/cs_1"),
                    200,
                    json!({"livemode": true, "payment_status": "paid", "amount_total": 900,
                           "created": 2_000_000_000, "customer_details": {"email": "c@buyer.com"},
                           "payment_intent": "pi_1"})
                    .to_string(),
                )
                .with(
                    &format!("{api}/charges/ch_nokey"),
                    401,
                    json!({"error": {"message": "Invalid API Key"}}).to_string(),
                ),
        );
        let pay = |id: &str| v.verify(&job(Some("first-payment"), Some(&root), vec![stripe(id)]));
        assert!(pay("ch_ok").verified());
        assert!(pay("pi_1").verified());
        assert!(pay("cs_1").verified());
        for (id, why) in [
            ("ch_test", "test-mode"),
            ("ch_ref", "refunded"),
            ("evt_1", "refunded"),
            ("evt_2", "not a payment"),
            ("ch_old", "before this speedrun"),
            ("ch_self", "committer"),
            ("ch_nokey", "rejected the stored key"),
            ("cus_1", "not a Stripe event"),
            ("ch_../../x", "not a Stripe id"),
        ] {
            let out = pay(id);
            assert!(out.problems.join(" ").contains(why), "{id}: {:?}", out.problems);
        }

        let no_key = Verifier {
            fetch: Box::new(Fake::default()),
            secrets: Box::new(Keys(&[("stripe", "rk_test_x")])),
        };
        let out = no_key.verify(&job(Some("first-payment"), None, vec![stripe("ch_ok")]));
        assert!(out.problems[0].contains("not a live-mode key"));
    }

    #[test]
    fn a_response_missing_a_field_the_api_always_sends_does_not_verify() {
        use serde_json::json;
        let (_d, root, _) = repo();
        let api = "https://api.stripe.com/v1";
        let without = |field: &str| {
            let mut c: Value = serde_json::from_str(&charge("ch_x", json!({}))).unwrap();
            c.as_object_mut().unwrap().remove(field);
            c.to_string()
        };
        let v = verifier(
            Fake::default()
                .with(&format!("{api}/charges/ch_norefund"), 200, without("refunded"))
                .with(&format!("{api}/charges/ch_nodispute"), 200, without("disputed")),
        );
        let pay = |id: &str| v.verify(&job(Some("first-payment"), Some(&root), vec![stripe(id)]));
        for (id, field) in [("ch_norefund", "refunded"), ("ch_nodispute", "disputed")] {
            let out = pay(id);
            assert!(!out.verified(), "{id} verified without {field}");
            assert!(
                out.problems.join(" ").contains(&format!("has no {field}")),
                "{id}: {:?}",
                out.problems
            );
        }
    }

    #[test]
    fn every_attached_item_must_pass() {
        let v = verifier(Fake::default().with("https://shop.acme.dev", 200, "<p>shop</p>"));
        let out = v.verify(&job(
            None,
            None,
            vec![url("https://shop.acme.dev"), url("https://gone.acme.dev")],
        ));
        assert!(!out.verified());
        assert_eq!(out.verifiers, vec!["url"]);
        assert_eq!(out.report().verifier, None);
    }

    // --- Audit regressions (docs/audit/v0.1.0.json, docs/V0.2.md).

    /// A repo whose only commit was made, by its committer date, in 2001.
    fn repo_committed_in_2001() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
                .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=f",
            "-c",
            "user.email=f@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "old",
        ]);
        let sha = git(&["rev-parse", "HEAD"]).trim().to_string();
        (dir, root, sha)
    }

    #[test]
    fn audit_a3_1_commit_from_before_the_run_does_not_verify() {
        let (_d, root, sha) = repo_committed_in_2001();
        let mut j = job(
            Some("happy-path-demo"),
            Some(&root),
            vec![Evidence::GitCommit { sha }],
        );
        j.run_started_at = DateTime::from_timestamp(1_600_000_000, 0).unwrap(); // 2020
        assert!(!verifier(Fake::default()).verify(&j).verified());
    }

    #[test]
    fn guard_commit_made_during_the_run_verifies_a_demo() {
        let (_d, root, sha) = repo();
        let j = job(
            Some("happy-path-demo"),
            Some(&root),
            vec![Evidence::GitCommit { sha }],
        );
        assert!(verifier(Fake::default()).verify(&j).verified());
    }

    #[test]
    fn semantic_policy_does_not_promote_git_metadata_to_proof() {
        let (_d, root, sha) = repo();
        let mut j = job(
            Some("happy-path-demo"),
            Some(&root),
            vec![Evidence::GitCommit { sha }],
        );
        j.policy = Some(VerificationPolicy::SemanticClaim);
        assert!(!verifier(Fake::default()).verify(&j).verified());
    }

    #[test]
    fn local_test_reference_requires_trusted_validation() {
        let mut j = job(
            Some("tests-green"),
            None,
            vec![Evidence::LocalTest {
                observation_id: "arbitrary".into(),
            }],
        );
        j.policy = Some(VerificationPolicy::LocalTestsOrCi);
        let v = verifier(Fake::default());
        assert!(!v.verify(&j).verified());
        j.local_test_verified = true;
        assert!(v.verify(&j).verified());
        j.evidence.push(Evidence::LocalTest {
            observation_id: "other".into(),
        });
        assert!(!v.verify(&j).verified());
    }

    #[test]
    fn public_scope_reports_local_observation_unavailable() {
        let mut j = job(
            Some("tests-green"),
            None,
            vec![Evidence::LocalTest {
                observation_id: "obs".into(),
            }],
        );
        j.policy = Some(VerificationPolicy::LocalTestsOrCi);
        j.scope = VerificationScope::Public;
        j.local_test_verified = true;
        let report = verifier(Fake::default()).verify(&j).report();
        assert_eq!(report.status, VerificationStatus::Unavailable);
        assert_eq!(report.scope, VerificationScope::Public);
        assert!(!report.verified);
        assert!(report.facts.is_empty());
    }

    /// A passing GitHub Actions run, created in 2030, that tested `sha` of Acme/Shop.
    fn passing_ci_run(sha: &str) -> (Verifier, Evidence) {
        let v = verifier(Fake::default().with(
            "https://api.github.com/repos/Acme/Shop/actions/runs/7",
            200,
            ci_body("success", sha, "2030-01-01T00:00:00Z"),
        ));
        let ev = Evidence::CiRun {
            url: "https://github.com/Acme/Shop/actions/runs/7".into(),
        };
        (v, ev)
    }

    /// M6.b: a `ci_run` whose head commit predates run start does not verify (V0.2 M6: reject the ci_run head_sha whose committer time is before run start).
    #[test]
    fn audit_m6_b_ci_run_on_a_commit_from_before_the_run_does_not_verify() {
        let (_d, root, sha) = repo_committed_in_2001();
        git_output(
            &root,
            &["remote", "add", "origin", "git@github.com:Acme/Shop.git"],
        )
        .expect("git remote");
        let (v, ev) = passing_ci_run(&sha);
        let mut j = job(Some("tests-green"), Some(&root), vec![ev]);
        j.run_started_at = DateTime::from_timestamp(1_600_000_000, 0).unwrap(); // 2020
        let out = v.verify(&j);
        assert!(!out.verified(), "CI ran in 2030 on a 2001 commit: {out:?}");
    }

    /// M6.b guard: a passing CI run on a commit made during the run still verifies.
    #[test]
    fn guard_m6_b_ci_run_on_a_commit_made_during_the_run_verifies() {
        let (_d, root, sha) = repo();
        let (v, ev) = passing_ci_run(&sha);
        let out = v.verify(&job(Some("tests-green"), Some(&root), vec![ev]));
        assert!(out.verified(), "{out:?}");
    }

    // --- W2.a: first-revenue and legal-pages rules, fixed in place (owner decision D4).

    const STRIPE_API: &str = "https://api.stripe.com/v1";

    fn stripe_object(base: Value, extra: Value) -> String {
        let mut o = base;
        o.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        o.to_string()
    }

    fn invoice(id: &str, extra: Value) -> String {
        stripe_object(
            serde_json::json!({
                "id": id, "object": "invoice", "livemode": true, "status": "paid",
                "amount_paid": 900, "paid_out_of_band": false, "created": 2_000_000_000,
                "status_transitions": {"paid_at": 2_000_000_000},
                "customer_email": "customer@buyer.com"
            }),
            extra,
        )
    }

    /// Charges and PaymentIntents the Stripe scenarios below resolve to.
    fn stripe_fake() -> Fake {
        use serde_json::json;
        Fake::default()
            .with(
                &format!("{STRIPE_API}/charges/ch_ok"),
                200,
                charge("ch_ok", json!({})),
            )
            .with(
                &format!("{STRIPE_API}/charges/ch_ref"),
                200,
                charge("ch_ref", json!({"refunded": true, "amount_refunded": 900})),
            )
            .with(
                &format!("{STRIPE_API}/payment_intents/pi_ok"),
                200,
                json!({"livemode": true, "status": "succeeded", "latest_charge": "ch_ok"}).to_string(),
            )
            .with(
                &format!("{STRIPE_API}/payment_intents/pi_ref"),
                200,
                json!({"livemode": true, "status": "succeeded", "latest_charge": "ch_ref"}).to_string(),
            )
    }

    fn first_payment(v: &Verifier, root: &str, id: &str) -> Outcome {
        v.verify(&job(Some("first-payment"), Some(root), vec![stripe(id)]))
    }

    #[test]
    fn audit_w2_a_invoice_paid_out_of_band_does_not_verify() {
        let (_d, root, _) = repo();
        let v = verifier(stripe_fake().with(
            &format!("{STRIPE_API}/invoices/in_oob"),
            200,
            invoice(
                "in_oob",
                serde_json::json!({"paid_out_of_band": true, "charge": "ch_ok"}),
            ),
        ));
        let out = first_payment(&v, &root, "in_oob");
        assert!(!out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_invoice_with_no_charge_fails_closed() {
        let (_d, root, _) = repo();
        let v = verifier(stripe_fake().with(
            &format!("{STRIPE_API}/invoices/in_bare"),
            200,
            invoice("in_bare", serde_json::json!({})),
        ));
        let out = first_payment(&v, &root, "in_bare");
        assert!(!out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_invoice_resolves_its_charge_through_its_payments() {
        // Current Stripe API versions list an invoice's payments instead of a `charge` field.
        let (_d, root, _) = repo();
        let paid_by = |pi: &str| {
            serde_json::json!({"payments": {"data": [
                {"payment": {"type": "payment_intent", "payment_intent": pi}}
            ]}})
        };
        let v = verifier(
            stripe_fake()
                .with(
                    &format!("{STRIPE_API}/invoices/in_refunded"),
                    200,
                    invoice("in_refunded", paid_by("pi_ref")),
                )
                .with(
                    &format!("{STRIPE_API}/invoices/in_paid"),
                    200,
                    invoice("in_paid", paid_by("pi_ok")),
                ),
        );
        let refunded = first_payment(&v, &root, "in_refunded");
        assert!(
            refunded.problems.join(" ").contains("refunded"),
            "{:?}",
            refunded.problems
        );
        assert!(first_payment(&v, &root, "in_paid").verified());
    }

    #[test]
    fn guard_w2_a_invoice_with_a_good_charge_verifies() {
        let (_d, root, _) = repo();
        let v = verifier(stripe_fake().with(
            &format!("{STRIPE_API}/invoices/in_card"),
            200,
            invoice("in_card", serde_json::json!({"charge": "ch_ok"})),
        ));
        let out = first_payment(&v, &root, "in_card");
        assert!(out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_checkout_session_without_a_payment_fails_closed() {
        let (_d, root, _) = repo();
        let v = verifier(
            stripe_fake().with(
                &format!("{STRIPE_API}/checkout/sessions/cs_bare"),
                200,
                serde_json::json!({"livemode": true, "payment_status": "paid", "amount_total": 900,
                       "created": 2_000_000_000, "customer_details": {"email": "c@buyer.com"}})
                .to_string(),
            ),
        );
        let out = first_payment(&v, &root, "cs_bare");
        assert!(!out.verified(), "{:?}", out.problems);
    }

    /// One API request: its URL and headers.
    type Request = (String, Vec<(String, String)>);

    /// Records the headers of every API request it answers.
    struct Recording {
        inner: Fake,
        seen: Arc<Mutex<Vec<Request>>>,
    }

    impl Fetch for Recording {
        fn get(&self, url: &str, headers: &[(&str, &str)], api: bool) -> Result<Page, String> {
            if api {
                let headers = headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                self.seen.lock().unwrap().push((url.to_string(), headers));
            }
            self.inner.get(url, &[], api)
        }
    }

    #[test]
    fn audit_w2_a_stripe_requests_pin_the_api_version() {
        let (_d, root, _) = repo();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let v = Verifier {
            fetch: Box::new(Recording {
                inner: stripe_fake(),
                seen: Arc::clone(&seen),
            }),
            secrets: Box::new(Keys(&[("stripe", "rk_live_x")])),
        };
        assert!(first_payment(&v, &root, "pi_ok").verified());
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty());
        for (url, headers) in seen.iter() {
            assert!(
                headers
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("stripe-version") && !v.trim().is_empty()),
                "{url} sent no Stripe-Version: {headers:?}"
            );
        }
    }

    #[test]
    fn audit_w2_a_founder_paying_under_a_plus_alias_is_a_committer() {
        let (_d, root, _) = repo();
        let v = verifier(Fake::default().with(
            &format!("{STRIPE_API}/charges/ch_alias"),
            200,
            charge(
                "ch_alias",
                serde_json::json!({"billing_details": {"email": "founder+shop@example.com"}}),
            ),
        ));
        let out = first_payment(&v, &root, "ch_alias");
        assert!(out.problems.join(" ").contains("committer"), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_charge_without_a_customer_email_does_not_verify() {
        let (_d, root, _) = repo();
        let v = verifier(Fake::default().with(
            &format!("{STRIPE_API}/charges/ch_anon"),
            200,
            charge(
                "ch_anon",
                serde_json::json!({"billing_details": {"email": null}, "receipt_email": null}),
            ),
        ));
        let out = first_payment(&v, &root, "ch_anon");
        assert!(!out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_mostly_refunded_charge_counts_as_refunded() {
        let (_d, root, _) = repo();
        let v = verifier(
            Fake::default()
                .with(
                    &format!("{STRIPE_API}/charges/ch_half"),
                    200,
                    charge("ch_half", serde_json::json!({"amount_refunded": 450})),
                )
                .with(
                    &format!("{STRIPE_API}/charges/ch_most"),
                    200,
                    charge("ch_most", serde_json::json!({"amount_refunded": 899})),
                ),
        );
        for id in ["ch_half", "ch_most"] {
            let out = first_payment(&v, &root, id);
            assert!(
                out.problems.join(" ").contains("refunded"),
                "{id}: {:?}",
                out.problems
            );
        }
    }

    fn legal_page(heading: &str) -> String {
        format!("<h1>{heading}</h1><p>{}</p>", "words ".repeat(MIN_LEGAL_WORDS))
    }

    fn legal(v: &Verifier, deployed: Option<&str>, urls: &[&str]) -> Outcome {
        let mut j = job(Some("legal-pages"), None, urls.iter().map(|u| url(u)).collect());
        j.deployed_url = deployed.map(str::to_string);
        v.verify(&j)
    }

    #[test]
    fn audit_w2_a_footer_links_do_not_make_legal_pages() {
        let marketing = format!(
            "<h1>Buy our widget</h1><p>{}</p><footer><a href=/terms>Terms of Service</a> \
             <a href=/privacy>Privacy Policy</a></footer>",
            "widget ".repeat(MIN_LEGAL_WORDS)
        );
        let v = verifier(
            Fake::default()
                .with("https://acme.dev/", 200, marketing.clone())
                .with("https://acme.dev/pricing", 200, marketing),
        );
        let out = legal(
            &v,
            Some("https://acme.dev"),
            &["https://acme.dev/", "https://acme.dev/pricing"],
        );
        assert!(!out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_nav_links_do_not_make_a_privacy_policy() {
        let home = format!(
            "<nav><a href=/privacy>Privacy Policy</a></nav><h1>Acme</h1><p>{}</p>",
            "widget ".repeat(MIN_LEGAL_WORDS)
        );
        let v = verifier(
            Fake::default()
                .with("https://acme.dev/terms", 200, legal_page("Terms of Service"))
                .with("https://acme.dev/home", 200, home),
        );
        let out = legal(
            &v,
            Some("https://acme.dev"),
            &["https://acme.dev/terms", "https://acme.dev/home"],
        );
        assert!(!out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_one_url_cannot_be_both_legal_pages() {
        let both = format!(
            "{}{}",
            legal_page("Terms of Service"),
            legal_page("Privacy Policy")
        );
        let v = verifier(Fake::default().with("https://acme.dev/legal", 200, both));
        let out = legal(&v, Some("https://acme.dev"), &["https://acme.dev/legal"]);
        assert!(!out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_legal_pages_on_another_domain_do_not_verify() {
        let v = verifier(
            Fake::default()
                .with(
                    "https://stripe.com/legal/ssa",
                    200,
                    legal_page("Terms of Service"),
                )
                .with(
                    "https://policies.google.com/privacy",
                    200,
                    legal_page("Privacy Policy"),
                ),
        );
        let out = legal(
            &v,
            Some("https://acme.dev"),
            &[
                "https://stripe.com/legal/ssa",
                "https://policies.google.com/privacy",
            ],
        );
        assert!(!out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn audit_w2_a_legal_pages_need_a_verified_deployment() {
        let v = verifier(
            Fake::default()
                .with("https://acme.dev/terms", 200, legal_page("Terms of Service"))
                .with("https://acme.dev/privacy", 200, legal_page("Privacy Policy")),
        );
        let out = legal(&v, None, &["https://acme.dev/terms", "https://acme.dev/privacy"]);
        assert!(!out.verified(), "{:?}", out.problems);
    }

    #[test]
    fn guard_w2_a_legal_pages_on_the_deployments_registrable_domain_verify() {
        let v = verifier(
            Fake::default()
                .with("https://www.acme.dev/terms", 200, legal_page("Terms of Service"))
                .with("https://www.acme.dev/privacy", 200, legal_page("Privacy Policy")),
        );
        let out = legal(
            &v,
            Some("https://app.acme.dev/dashboard"),
            &["https://www.acme.dev/terms", "https://www.acme.dev/privacy"],
        );
        assert!(out.verified(), "{:?}", out.problems);
    }
}
