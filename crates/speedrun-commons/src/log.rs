//! The log's public shape (COMMONS.md §19): the signed tree head and the
//! inclusion proof. Both the publication path and the read API ask this
//! module for those two objects, so the rule that turns leaf hashes into a
//! head or a proof lives in one place and no caller re-derives it.

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use ed25519_dalek::SigningKey;
use speedrun_core::commons::{InclusionProof, TreeHead, tree_head_digest};
use speedrun_core::{merkle, sign};

use crate::store::Store;

/// Why the log cannot answer a read.
#[derive(Debug)]
pub enum LogError {
    /// The leaf is not in the requested tree; answer 404.
    NotInTree,
    /// The requested tree is larger than the log; answer 400.
    BeyondLog,
    /// A stored leaf or the stored head is malformed; answer 500.
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for LogError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

/// The latest signed head, or the signed head of the empty log when nothing
/// has been appended. The empty head is derived, never stored, so a fresh
/// commons answers with a signed head just as a used one does.
pub async fn latest(store: &Store, key: &SigningKey) -> Result<TreeHead, LogError> {
    match store.latest_tree_head().await? {
        Some(body) => serde_json::from_str(&body).map_err(|error| LogError::Internal(error.into())),
        None => {
            if store.tree_size().await? != 0 {
                return Err(LogError::Internal(anyhow!("the log has leaves but no tree head")));
            }
            signed_head(key, &[], Utc::now()).map_err(LogError::Internal)
        }
    }
}

/// The proof for `index` against the tree of `size` leaves, or against the
/// whole log when `size` is absent.
pub async fn inclusion(store: &Store, index: u64, size: Option<u64>) -> Result<InclusionProof, LogError> {
    let leaves = store.leaf_hashes().await?;
    proof(&leaves, index, size)
}

/// The signed head of the tree whose leaves are `leaves`, issued at `at`.
/// `None` from `merkle::root` is malformed state built by the commons itself,
/// so it is an error, never a default root.
pub fn signed_head(key: &SigningKey, leaves: &[String], at: DateTime<Utc>) -> anyhow::Result<TreeHead> {
    let root_hash = merkle::root(leaves).ok_or_else(|| anyhow!("the log holds a malformed leaf hash"))?;
    let mut head = TreeHead {
        tree_size: u64::try_from(leaves.len())?,
        root_hash,
        issued_at: at,
        signature: String::new(),
    };
    let digest = tree_head_digest(&head)?;
    head.signature = sign::sign_hash(key, &digest);
    Ok(head)
}

/// The proof for `index` against the first `size` leaves, or all of them when
/// `size` is absent. An absent size is a normal state defined by §19, so it is
/// matched explicitly rather than defaulted.
pub fn proof(leaves: &[String], index: u64, size: Option<u64>) -> Result<InclusionProof, LogError> {
    let log_size = u64::try_from(leaves.len()).map_err(|error| LogError::Internal(error.into()))?;
    let tree_size = match size {
        Some(requested) => requested,
        None => log_size,
    };
    if tree_size > log_size {
        return Err(LogError::BeyondLog);
    }
    if index >= tree_size {
        return Err(LogError::NotInTree);
    }
    let tree_len = usize::try_from(tree_size).map_err(|error| LogError::Internal(error.into()))?;
    let tree_leaves = leaves
        .get(..tree_len)
        .ok_or_else(|| LogError::Internal(anyhow!("tree size {tree_size} is outside the log")))?;
    let position = usize::try_from(index).map_err(|_| LogError::NotInTree)?;
    let leaf_hash = tree_leaves.get(position).cloned().ok_or(LogError::NotInTree)?;
    let audit_path = merkle::inclusion_proof(index, tree_leaves)
        .ok_or_else(|| LogError::Internal(anyhow!("a stored leaf hash is malformed")))?;
    Ok(InclusionProof {
        leaf_index: index,
        tree_size,
        leaf_hash,
        audit_path,
    })
}
