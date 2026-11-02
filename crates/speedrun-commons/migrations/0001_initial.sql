-- The commons' record (COMMONS.md §13, P1 subset). Every row is written once
-- and never updated: corrections arrive as new publications.

-- Each published bundle, its body exactly as received.
CREATE TABLE bundles (
    digest      TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL,
    tier        INTEGER NOT NULL,
    body        TEXT NOT NULL,
    received_at TEXT NOT NULL
);
CREATE INDEX bundles_by_run ON bundles (run_id, received_at);

-- The signed judgment of each bundle, as JSON.
CREATE TABLE judgments (
    bundle_digest TEXT PRIMARY KEY REFERENCES bundles (digest),
    body          TEXT NOT NULL
);

-- The Merkle log: one leaf per publication, indices 0, 1, 2, ... in arrival order.
CREATE TABLE log_leaves (
    leaf_index    INTEGER PRIMARY KEY,
    bundle_digest TEXT NOT NULL UNIQUE REFERENCES bundles (digest),
    leaf_hash     TEXT NOT NULL
);

-- Every signed tree head issued, as JSON.
CREATE TABLE tree_heads (
    tree_size INTEGER PRIMARY KEY,
    body      TEXT NOT NULL
);
