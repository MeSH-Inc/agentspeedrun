//! The commons' transparency log (COMMONS.md §8) as a Merkle tree in the
//! shape of RFC 9162 §2.1: leaf hashes are `SHA-256(0x00 || data)`, interior
//! nodes `SHA-256(0x01 || left || right)`, a tree of `n` leaves splits at the
//! largest power of two below `n`, and the empty tree's root is `SHA-256("")`.
//! Hashes cross this API as lowercase hex. No I/O: the server stores leaves,
//! and anyone can check a proof with this module or their own code.

use sha2::{Digest, Sha256};

/// A Merkle hash, kept as bytes inside this module. The public API crosses as
/// lowercase hex, like the rest of speedrun-core.
type Hash = [u8; 32];

const LEAF_PREFIX: [u8; 1] = [0x00];
const NODE_PREFIX: [u8; 1] = [0x01];

/// The leaf hash of one publication: the leaf data is the UTF-8 text
/// `<bundle_digest>:<judgment_digest>` (both hex), hashed as RFC 9162 hashes a
/// leaf.
pub fn leaf_hash(bundle_digest: &str, judgment_digest: &str) -> String {
    let data = format!("{bundle_digest}:{judgment_digest}");
    hex::encode(hash_leaf(data.as_bytes()))
}

/// The root of the tree whose leaves (hex leaf hashes) are `leaves`, in order.
/// `None` when a leaf is not a 32-byte hex hash.
pub fn root(leaves: &[String]) -> Option<String> {
    let hashes = parse_leaves(leaves)?;
    Some(hex::encode(root_hashes(&hashes)))
}

/// The audit path (RFC 9162 §2.1.3.1) proving leaf `index` in the tree of
/// `leaves`, ordered from the leaf's sibling up. `None` when `index` is not a
/// leaf of the tree.
pub fn inclusion_proof(index: u64, leaves: &[String]) -> Option<Vec<String>> {
    // An index past the tree, however large, names no leaf.
    let index = usize::try_from(index)
        .ok()
        .filter(|index| *index < leaves.len())?;
    let hashes = parse_leaves(leaves)?;
    let proof = audit_path(index, &hashes);
    Some(proof.iter().map(hex::encode).collect())
}

/// Whether `proof` shows that `leaf` (a hex leaf hash) is leaf `index` of the
/// tree of `tree_size` leaves whose root is `root` (RFC 9162 §2.1.3.2).
/// Malformed input does not verify.
pub fn verify_inclusion(leaf: &str, index: u64, tree_size: u64, proof: &[String], root: &str) -> bool {
    if tree_size == 0 {
        return false;
    }
    let Ok(index) = usize::try_from(index) else {
        return false;
    };
    let Ok(tree_size) = usize::try_from(tree_size) else {
        return false;
    };
    if index >= tree_size {
        return false;
    }
    let Some(leaf) = parse_hash(leaf) else {
        return false;
    };
    let Some(expected_root) = parse_hash(root) else {
        return false;
    };
    let mut audit = Vec::with_capacity(proof.len());
    for item in proof {
        let Some(hash) = parse_hash(item) else {
            return false;
        };
        audit.push(hash);
    }
    let mut cursor = 0;
    let Some(computed) = recompute_root(index, tree_size, leaf, &audit, &mut cursor) else {
        return false;
    };
    cursor == audit.len() && computed == expected_root
}

/// SHA-256 with the RFC 9162 leaf prefix.
fn hash_leaf(data: &[u8]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(LEAF_PREFIX);
    hasher.update(data);
    finish(hasher)
}

/// SHA-256 with the RFC 9162 interior-node prefix.
fn hash_node(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(NODE_PREFIX);
    hasher.update(left);
    hasher.update(right);
    finish(hasher)
}

/// The RFC 9162 root of an empty tree: SHA-256 of the empty string.
fn empty_root() -> Hash {
    finish(Sha256::new())
}

fn finish(hasher: Sha256) -> Hash {
    let digest = hasher.finalize();
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(digest.iter()) {
        *slot = *byte;
    }
    hash
}

/// The root of `hashes`, which are already leaf hashes.
fn root_hashes(hashes: &[Hash]) -> Hash {
    match hashes {
        [] => empty_root(),
        [only] => *only,
        _ => {
            let (left, right) = hashes.split_at(split(hashes.len()));
            hash_node(&root_hashes(left), &root_hashes(right))
        }
    }
}

/// The largest power of two smaller than `n` (`n > 1`).
fn split(n: usize) -> usize {
    1usize << (n - 1).ilog2()
}

/// The RFC 9162 audit path for `index`, whose leaves are already hashes.
fn audit_path(index: usize, hashes: &[Hash]) -> Vec<Hash> {
    let n = hashes.len();
    if n <= 1 {
        return Vec::new();
    }
    let k = split(n);
    let (left, right) = hashes.split_at(k);
    if index < k {
        let mut proof = audit_path(index, left);
        proof.push(root_hashes(right));
        proof
    } else {
        let mut proof = audit_path(index - k, right);
        proof.push(root_hashes(left));
        proof
    }
}

/// Rebuild a root from a leaf and the audit path, following RFC 9162 §2.1.3.2.
/// `cursor` advances through `proof`; the caller checks it reached the end.
fn recompute_root(
    index: usize,
    tree_size: usize,
    leaf: Hash,
    proof: &[Hash],
    cursor: &mut usize,
) -> Option<Hash> {
    if index >= tree_size {
        return None;
    }
    if tree_size == 1 {
        return Some(leaf);
    }
    let k = split(tree_size);
    if index < k {
        let left = recompute_root(index, k, leaf, proof, cursor)?;
        let right = take_next(proof, cursor)?;
        Some(hash_node(&left, &right))
    } else {
        let right = recompute_root(index - k, tree_size - k, leaf, proof, cursor)?;
        let left = take_next(proof, cursor)?;
        Some(hash_node(&left, &right))
    }
}

fn take_next(proof: &[Hash], cursor: &mut usize) -> Option<Hash> {
    let hash = proof.get(*cursor).copied()?;
    *cursor += 1;
    Some(hash)
}

/// Parse a lowercase or uppercase hex hash into its bytes. `None` for
/// anything that is not exactly 32 bytes of hex.
fn parse_hash(hex_hash: &str) -> Option<Hash> {
    let Ok(bytes) = hex::decode(hex_hash) else {
        return None;
    };
    if bytes.len() != 32 {
        return None;
    }
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(bytes.iter()) {
        *slot = *byte;
    }
    Some(hash)
}

fn parse_leaves(leaves: &[String]) -> Option<Vec<Hash>> {
    let mut hashes = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        hashes.push(parse_hash(leaf)?);
    }
    Some(hashes)
}
