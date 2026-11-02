//! The commons' transparency log (COMMONS.md §8) as a Merkle tree in the
//! shape of RFC 9162 §2.1: leaf hashes are `SHA-256(0x00 || data)`, interior
//! nodes `SHA-256(0x01 || left || right)`, a tree of `n` leaves splits at the
//! largest power of two below `n`, and the empty tree's root is `SHA-256("")`.
//! Hashes cross this API as lowercase hex. No I/O: the server stores leaves,
//! and anyone can check a proof with this module or their own code.

/// The leaf hash of one publication: the leaf data is the UTF-8 text
/// `<bundle_digest>:<judgment_digest>` (both hex), hashed as RFC 9162 hashes a
/// leaf.
pub fn leaf_hash(bundle_digest: &str, judgment_digest: &str) -> String {
    let _ = (bundle_digest, judgment_digest);
    String::new()
}

/// The root of the tree whose leaves (hex leaf hashes) are `leaves`, in order.
pub fn root(leaves: &[String]) -> String {
    let _ = leaves;
    String::new()
}

/// The audit path (RFC 9162 §2.1.3.1) proving leaf `index` in the tree of
/// `leaves`, ordered from the leaf's sibling up. `None` when `index` is not a
/// leaf of the tree.
pub fn inclusion_proof(index: u64, leaves: &[String]) -> Option<Vec<String>> {
    let _ = (index, leaves);
    None
}

/// Whether `proof` shows that `leaf` (a hex leaf hash) is leaf `index` of the
/// tree of `tree_size` leaves whose root is `root` (RFC 9162 §2.1.3.2).
/// Malformed input does not verify.
pub fn verify_inclusion(leaf: &str, index: u64, tree_size: u64, proof: &[String], root: &str) -> bool {
    let _ = (leaf, index, tree_size, proof, root);
    false
}
