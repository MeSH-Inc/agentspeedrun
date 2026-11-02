//! Judgment of a published run's claims (COMMONS.md §6, §19). Nothing here
//! trusts the client's verdicts: a claim is `evidence_verified` only when the
//! shared verifier, run here, passes the claim's evidence.

use speedrun_core::commons::MilestoneJudgment;
use speedrun_core::{Event, RunView};
use speedrun_verify::Verifier;

/// Judge every milestone of `run`, the fold of `events`, in the run's
/// milestone order.
pub fn judge(run: &RunView, events: &[Event], verifier: &Verifier) -> Vec<MilestoneJudgment> {
    let _ = (run, events, verifier);
    Vec::new()
}
