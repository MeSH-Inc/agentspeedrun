-- GitHub identity (COMMONS.md §20). A pending link is one device-flow
-- authorization in progress; a link is a device key both the device and the
-- commons signed for one person. Links are stored rows, append-only, and are
-- never Merkle leaves, so the leaf format stays `bundle:judgment`.

CREATE TABLE pending_links (
    link_id       TEXT PRIMARY KEY,
    device_pubkey TEXT NOT NULL,
    device_code   TEXT NOT NULL,
    expires_at    TEXT NOT NULL,
    state         TEXT NOT NULL,
    github_id     INTEGER,
    login         TEXT,
    authorized_at TEXT
);

-- A login names at most one person: the one who most recently authorized
-- with it. A person whose login was later taken keeps their id and links, with
-- no login.
CREATE TABLE people (
    github_id INTEGER PRIMARY KEY,
    login     TEXT
);
CREATE UNIQUE INDEX people_by_login ON people (lower(login)) WHERE login IS NOT NULL;

CREATE TABLE links (
    link_id       TEXT PRIMARY KEY,
    github_id     INTEGER NOT NULL,
    login         TEXT NOT NULL,
    device_pubkey TEXT NOT NULL,
    body          TEXT NOT NULL,
    linked_at     TEXT NOT NULL
);
CREATE INDEX links_by_person ON links (github_id);
CREATE INDEX links_by_device ON links (device_pubkey);

-- Which key signed each stored bundle, so a person's publications can be
-- found from the links without parsing every bundle body.
ALTER TABLE bundles ADD COLUMN publisher_pubkey TEXT;
UPDATE bundles SET publisher_pubkey = json_extract(body, '$.publisher_pubkey');
CREATE INDEX bundles_by_publisher ON bundles (publisher_pubkey);
