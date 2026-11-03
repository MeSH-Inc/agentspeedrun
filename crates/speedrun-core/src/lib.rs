//! Core domain for agentspeedrun: the event log contract, hash chain,
//! category rulesets, run projections and timing.
//!
//! Everything a run "is" is derived by folding its append-only event log.

pub mod api;
pub mod bundle;
pub mod category;
pub mod commons;
pub mod digest;
pub mod event;
pub mod hash;
pub mod live;
pub mod merkle;
pub mod paths;
pub mod preferences;
pub mod projection;
pub mod redact;
pub mod sign;

pub use category::{Category, MilestoneTemplate};
pub use event::{
    Claimant, Event, EventPayload, Evidence, MilestoneOrigin, RunOutcome, SCHEMA_VERSION, Trust,
};
pub use projection::{MilestoneState, MilestoneView, RunStatus, RunView, Timing};

/// Generate a new sortable, globally unique id.
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}
