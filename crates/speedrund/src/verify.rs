//! Evidence verifiers. A milestone reaches `evidence_verified` only when the
//! daemon itself has checked every piece of verifiable evidence attached,
//! and the evidence covers what the milestone's category key asks for.
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

use chrono::{DateTime, Utc};
use serde_json::Value;
use speedrun_core::{Evidence, api::VerificationReport, paths};

/// At most this many network-checked items per claim.
const MAX_REMOTE_EVIDENCE: usize = 6;
/// Bodies beyond this are truncated before inspection.
const MAX_BODY: u64 = 2 * 1024 * 1024;
/// A legal page needs at least this much server-rendered text.
const MIN_LEGAL_WORDS: usize = 200;

/// Everything a verification needs, captured under the store lock.
#[derive(Debug, Clone)]
pub struct Job {
    pub run_id: String,
    pub cause_event_id: String,
    pub milestone_id: String,
    /// Category key, for predefined milestones.
    pub key: Option<String>,
    pub evidence: Vec<Evidence>,
    pub project_root: Option<String>,
    pub run_started_at: DateTime<Utc>,
    pub claimed_at: DateTime<Utc>,
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
        Evidence::StripeEvent { .. } => "stripe_event",
        Evidence::Note { .. } => "note",
    }
}

impl Verifier {
    /// Check every piece of evidence. `note` evidence is context, not proof.
    pub fn verify(&self, job: &Job) -> Outcome {
        let mut out = Outcome::default();
        if let Some((needed, what)) = required_kind(job.key.as_deref())
            && !job.evidence.iter().any(|e| kind(e) == needed)
        {
            out.problems.push(format!("this milestone is verified by {what}"));
        }
        let remote = job
            .evidence
            .iter()
            .filter(|e| !matches!(e, Evidence::Note { .. } | Evidence::GitCommit { .. }))
            .count();
        if remote > MAX_REMOTE_EVIDENCE {
            out.problems.push(format!(
                "at most {MAX_REMOTE_EVIDENCE} URLs, CI runs and payments per claim"
            ));
            return out;
        }

        let mut legal = LegalCoverage::default();
        for ev in &job.evidence {
            let (name, result) = match ev {
                Evidence::Note { .. } => continue,
                Evidence::GitCommit { sha } => (
                    "git",
                    match job.project_root.as_deref() {
                        Some(root) if git_commit_in_history(root, sha) => Ok(()),
                        Some(_) => Err(format!("commit {sha} is not in this project's history")),
                        None => Err("the run has no project root to check commits against".into()),
                    },
                ),
                Evidence::Url { url } => ("url", self.check_url(url, job.key.as_deref(), &mut legal)),
                Evidence::CiRun { url } => ("ci", self.check_ci(url, job)),
                Evidence::StripeEvent { event_id } => ("stripe", self.check_stripe(event_id, job)),
            };
            match result {
                Ok(()) => out.verifiers.push(name),
                Err(e) => out.problems.push(e),
            }
        }
        // Coverage only means something once every page loaded.
        if job.key.as_deref() == Some("legal-pages") && out.problems.is_empty() {
            out.problems.extend(legal.missing());
        }
        out.verifiers.sort_unstable();
        out.verifiers.dedup();
        out
    }

    // --- URLs ------------------------------------------------------------

    fn check_url(&self, url: &str, key: Option<&str>, legal: &mut LegalCoverage) -> Result<(), String> {
        public_https_url(url)?;
        let page = self.fetch.get(url, &[], false)?;
        public_https_url(&page.url).map_err(|e| format!("{url} redirects to {}: {e}", page.url))?;
        if !(200..300).contains(&page.status) {
            return Err(format!("{url} answered HTTP {}", page.status));
        }
        let text = visible_text(&page.body).to_lowercase();
        let short = text.split_whitespace().count() < PLACEHOLDER_MAX_WORDS;
        let sign = PLACEHOLDER_SIGNS.iter().find(|s| short && text.contains(*s));
        if let Some(sign) = sign.or(["lorem ipsum"].iter().find(|s| text.contains(*s))) {
            return Err(format!("{url} looks like a placeholder page (\"{sign}\")"));
        }
        if key == Some("legal-pages") {
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
        let field = |k: &str| run.get(k).and_then(Value::as_str).unwrap_or_default();
        if field("status") != "completed" {
            return Err(format!(
                "CI run {id} has not finished (status {})",
                field("status")
            ));
        }
        if field("conclusion") != "success" {
            return Err(format!("CI run {id} concluded {}", field("conclusion")));
        }
        let sha = field("head_sha");
        if !git_commit_in_history(root, sha) {
            return Err(format!(
                "CI run {id} tested {sha}, which is not in this project's history"
            ));
        }
        let created = DateTime::parse_from_rfc3339(field("created_at"))
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
            founders: job
                .project_root
                .as_deref()
                .map(founder_emails)
                .unwrap_or_default(),
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

#[derive(Default)]
struct LegalCoverage {
    terms: bool,
    privacy: bool,
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
        self.terms |= terms;
        self.privacy |= privacy;
        Ok(())
    }

    fn missing(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.terms {
            out.push("no terms of service page among the URLs".into());
        }
        if !self.privacy {
            out.push("no privacy policy page among the URLs".into());
        }
        out
    }
}

fn api_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "no details".into())
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

fn git_output(root: &str, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
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

/// Everyone who has committed to the project: payments from them are not revenue.
fn founder_emails(root: &str) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = git_output(root, &["log", "--format=%ae%n%ce"])
        .unwrap_or_default()
        .lines()
        .chain(
            git_output(root, &["config", "user.email"])
                .as_deref()
                .unwrap_or_default()
                .lines(),
        )
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| e.contains('@'))
        .collect();
    out.remove("noreply@github.com");
    out
}

// --- Stripe helpers ---------------------------------------------------------

struct Stripe<'a> {
    fetch: &'a dyn Fetch,
    auth: String,
    /// Payments must be made after the run started (unix seconds).
    since: i64,
    founders: BTreeSet<String>,
}

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
        let page = self.fetch.get(&url, &[("Authorization", &self.auth)], true)?;
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
        let prefix = id.split('_').next().unwrap_or_default();
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
        let s = |k: &str| obj.get(k).and_then(Value::as_str).unwrap_or_default();
        let n = |k: &str| obj.get(k).and_then(Value::as_i64).unwrap_or_default();
        let linked = |k: &str| {
            obj.get(k)
                .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(Value::as_str)))
                .map(str::to_string)
        };
        match prefix {
            "evt" => {
                if !STRIPE_PAYMENT_EVENTS.contains(&s("type")) {
                    return Err(format!("{id} is a {} event, not a payment", s("type")));
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
                if s("status") != "succeeded" {
                    return Err(format!("payment {id} is {}", s("status")));
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
                if s("payment_status") != "paid" || n("amount_total") <= 0 {
                    return Err(format!("checkout session {id} is not paid"));
                }
                self.not_founder(id, obj.pointer("/customer_details/email"))?;
                self.after_start(id, n("created"))?;
                match linked("payment_intent").or_else(|| linked("invoice")) {
                    Some(next) => self.check(&next, depth + 1),
                    None => Ok(()),
                }
            }
            "in" => {
                if s("status") != "paid" || n("amount_paid") <= 0 {
                    return Err(format!("invoice {id} is not paid"));
                }
                self.not_founder(id, obj.get("customer_email"))?;
                let paid_at = obj
                    .pointer("/status_transitions/paid_at")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(|| n("created"));
                self.after_start(id, paid_at)?;
                match linked("charge") {
                    Some(ch) => self.check(&ch, depth + 1),
                    None => Ok(()),
                }
            }
            _ => unreachable!("prefix checked above"),
        }
    }

    fn check_charge(&self, id: &str, ch: &Value) -> Result<(), String> {
        let b = |k: &str| ch.get(k).and_then(Value::as_bool).unwrap_or_default();
        let n = |k: &str| ch.get(k).and_then(Value::as_i64).unwrap_or_default();
        if !b("paid") || ch.get("status").and_then(Value::as_str) != Some("succeeded") || !b("captured") {
            return Err(format!("charge {id} did not succeed"));
        }
        let captured = if n("amount_captured") > 0 {
            n("amount_captured")
        } else {
            n("amount")
        };
        if captured <= 0 {
            return Err(format!("charge {id} captured nothing"));
        }
        if b("refunded") || n("amount_refunded") >= captured {
            return Err(format!("charge {id} was refunded"));
        }
        if b("disputed") {
            return Err(format!("charge {id} is disputed"));
        }
        self.after_start(id, n("created"))?;
        self.not_founder(id, ch.pointer("/billing_details/email"))?;
        self.not_founder(id, ch.get("receipt_email"))
    }

    fn after_start(&self, id: &str, created: i64) -> Result<(), String> {
        if created < self.since {
            return Err(format!("{id} was paid before this speedrun started"));
        }
        Ok(())
    }

    fn not_founder(&self, id: &str, email: Option<&Value>) -> Result<(), String> {
        match email.and_then(Value::as_str).map(str::to_ascii_lowercase) {
            Some(e) if self.founders.contains(&e) => Err(format!(
                "{id} was paid by a committer to this project, not a customer"
            )),
            _ => Ok(()),
        }
    }
}

// --- real network -----------------------------------------------------------

/// HTTPS client for verifiers. Refuses to connect to private addresses
/// (unless a proxy does the connecting) and reads at most `MAX_BODY`.
pub struct Net {
    pages: ureq::Agent,
    api: ureq::Agent,
}

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
    pub fn new() -> Self {
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
        Self {
            pages: build(5),
            api: build(0),
        }
    }
}

impl Fetch for Net {
    fn get(&self, url: &str, headers: &[(&str, &str)], api: bool) -> Result<Page, String> {
        let agent = if api { &self.api } else { &self.pages };
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
                    t.message().unwrap_or(&t.kind().to_string())
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
            run_id: "r".into(),
            cause_event_id: "e".into(),
            milestone_id: key.unwrap_or("m").into(),
            key: key.map(str::to_string),
            evidence,
            project_root: root.map(str::to_string),
            run_started_at: DateTime::from_timestamp(1_000_000, 0).unwrap(),
            claimed_at: Utc::now(),
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
}
