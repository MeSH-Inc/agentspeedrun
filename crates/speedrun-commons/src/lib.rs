//! The commons server (COMMONS.md §7, §8; phase P1). It stores each published
//! bundle verbatim, judges the bundle's claims with the same verifier the
//! daemon previews with, signs the judgment, and anchors the publication as a
//! leaf of a Merkle log with signed tree heads.
//!
//! The contract is COMMONS.md §19.

pub mod http;
pub mod judge;
pub mod keys;
pub mod log;
pub mod record;
pub mod store;
