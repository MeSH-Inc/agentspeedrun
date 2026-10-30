//! Credentials for the daemon's verifiers, one file per name under
//! `$SPEEDRUN_HOME/secrets`, readable only by the owner. The daemon reads
//! them at verification time, so no restart is needed.

use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use speedrun_core::paths;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Name {
    /// Restricted live-mode key (rk_live_…) with read access to Charges,
    /// PaymentIntents, Checkout Sessions, Invoices and Events.
    Stripe,
    /// GitHub token that can read Actions runs (only needed for private
    /// repos when `gh auth login` has not been run).
    Github,
}

impl Name {
    fn file(self) -> &'static str {
        match self {
            Self::Stripe => "stripe",
            Self::Github => "github",
        }
    }
}

/// Check a value before storing it. Returns a warning to show, if any.
pub fn validate(name: Name, value: &str) -> Result<Option<&'static str>> {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        bail!("the value is empty or contains whitespace");
    }
    match name {
        Name::Stripe if value.starts_with("rk_live_") => Ok(None),
        Name::Stripe if value.starts_with("sk_live_") => Ok(Some(
            "this is a full secret key; the verifier only reads, so a restricted key (rk_live_…) is safer",
        )),
        Name::Stripe if value.contains("_test_") => {
            bail!("test-mode keys cannot see real revenue; use a live-mode restricted key (rk_live_…)")
        }
        Name::Stripe => bail!("not a Stripe live-mode key (expected rk_live_…)"),
        Name::Github => Ok(None),
    }
}

pub fn set(name: Name) -> Result<()> {
    let value = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("{} (input hidden): ", name.file()))?
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    };
    let value = value.trim();
    if let Some(warning) = validate(name, value)? {
        eprintln!("warning: {warning}");
    }
    let dir = paths::secrets_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let tmp = dir.join(format!(".{}.tmp", name.file()));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(value.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, dir.join(name.file()))?;
    println!(
        "Stored the {} credential; the daemon uses it from the next claim.",
        name.file()
    );
    Ok(())
}

pub fn remove(name: Name) -> Result<()> {
    match fs::remove_file(paths::secrets_dir().join(name.file())) {
        Ok(()) => println!("Removed the {} credential.", name.file()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("No {} credential stored.", name.file())
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub fn list() {
    for name in Name::value_variants() {
        let path = paths::secrets_dir().join(name.file());
        let state = match fs::read_to_string(&path) {
            Ok(_) => "stored",
            Err(_) => "not stored",
        };
        println!("{:<7} {state}", name.file());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripe_keys_must_be_live() {
        assert!(validate(Name::Stripe, "rk_live_abc").unwrap().is_none());
        assert!(validate(Name::Stripe, "sk_live_abc").unwrap().is_some());
        assert!(validate(Name::Stripe, "rk_test_abc").is_err());
        assert!(validate(Name::Stripe, "pk_live_abc").is_err());
        assert!(validate(Name::Github, "ghp abc").is_err());
    }
}
