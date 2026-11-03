-- The sequence is explicit and append-only. Each accepted digest has at most
-- one successor within a run, and the first digest has no predecessor.
CREATE TABLE publication_history (
    digest TEXT PRIMARY KEY REFERENCES bundles(digest),
    run_id TEXT NOT NULL,
    predecessor TEXT REFERENCES publication_history(digest),
    UNIQUE(predecessor)
);
CREATE UNIQUE INDEX publication_history_first ON publication_history(run_id) WHERE predecessor IS NULL;
INSERT INTO publication_history (digest, run_id, predecessor)
SELECT b.digest, b.run_id,
       (SELECT prior.bundle_digest FROM log_leaves prior
        JOIN bundles pb ON pb.digest = prior.bundle_digest
        WHERE pb.run_id = b.run_id AND prior.leaf_index < l.leaf_index
        ORDER BY prior.leaf_index DESC LIMIT 1)
FROM bundles b JOIN log_leaves l ON l.bundle_digest = b.digest ORDER BY l.leaf_index;
CREATE TRIGGER publication_history_no_update BEFORE UPDATE ON publication_history
BEGIN SELECT RAISE(ABORT, 'publication history is immutable'); END;
CREATE TRIGGER publication_history_no_delete BEFORE DELETE ON publication_history
BEGIN SELECT RAISE(ABORT, 'publication history is immutable'); END;
CREATE TRIGGER bundles_no_update BEFORE UPDATE ON bundles
BEGIN SELECT RAISE(ABORT, 'bundles are immutable'); END;
CREATE TRIGGER bundles_no_delete BEFORE DELETE ON bundles
BEGIN SELECT RAISE(ABORT, 'bundles are immutable'); END;
CREATE TRIGGER log_leaves_no_update BEFORE UPDATE ON log_leaves
BEGIN SELECT RAISE(ABORT, 'log leaves are immutable'); END;
CREATE TRIGGER log_leaves_no_delete BEFORE DELETE ON log_leaves
BEGIN SELECT RAISE(ABORT, 'log leaves are immutable'); END;
