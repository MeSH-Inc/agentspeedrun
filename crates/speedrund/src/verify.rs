//! Evidence verifiers. A milestone reaches `evidence_verified` only when the
//! daemon itself has checked every piece of verifiable evidence attached.

use std::process::Command;

use speedrun_core::Evidence;

/// Verify `evidence` against the project. Returns the verifier names used,
/// or `None` if nothing verifiable was attached or any check failed.
/// `note` evidence is context, not proof, and is ignored.
pub fn verify(project_root: Option<&str>, evidence: &[Evidence]) -> Option<Vec<&'static str>> {
    let mut used = Vec::new();
    for ev in evidence {
        match ev {
            Evidence::Note { .. } => continue,
            Evidence::GitCommit { sha } => {
                if !git_commit_in_history(project_root?, sha) {
                    return None;
                }
                used.push("git");
            }
            // Not implemented yet: these stay at the trust they were claimed with.
            Evidence::Url { .. } | Evidence::CiRun { .. } | Evidence::StripeEvent { .. } => return None,
        }
    }
    used.dedup();
    (!used.is_empty()).then_some(used)
}

/// The commit exists and is an ancestor of (or equal to) HEAD.
fn git_commit_in_history(root: &str, sha: &str) -> bool {
    let well_formed = (7..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
    if !well_formed {
        return false;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    git(&["cat-file", "-e", &format!("{sha}^{{commit}}")])
        && git(&["merge-base", "--is-ancestor", sha, "HEAD"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_hex_shas_without_running_git() {
        assert!(!git_commit_in_history("/", "--output=/tmp/x"));
        assert!(!git_commit_in_history("/", "abc"));
    }

    #[test]
    fn notes_alone_do_not_verify() {
        assert_eq!(
            verify(
                Some("/"),
                &[Evidence::Note {
                    text: "trust me".into()
                }]
            ),
            None
        );
    }

    #[test]
    fn verifies_real_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        let sha = String::from_utf8(
            Command::new("git")
                .args(["-C", root, "rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let ev = [Evidence::GitCommit {
            sha: sha.trim().into(),
        }];
        assert_eq!(verify(Some(root), &ev), Some(vec!["git"]));
        let bogus = [Evidence::GitCommit {
            sha: "deadbeefdeadbeef".into(),
        }];
        assert_eq!(verify(Some(root), &bogus), None);
    }
}
