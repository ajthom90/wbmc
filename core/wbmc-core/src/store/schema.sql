-- This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

-- Version 1 baseline. Later schema changes belong in migrations.rs.

CREATE TABLE schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    email TEXT NOT NULL,
    display_name TEXT NOT NULL,
    config TEXT NOT NULL
);

CREATE TABLE folders (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    remote_id TEXT NOT NULL,
    name TEXT NOT NULL,
    role TEXT NOT NULL,
    parent_id TEXT REFERENCES folders(id) ON DELETE CASCADE,
    unread_count INTEGER NOT NULL DEFAULT 0,
    total_count INTEGER NOT NULL DEFAULT 0,
    sync_state TEXT,
    UNIQUE (account_id, remote_id)
);

CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    remote_id TEXT NOT NULL,
    subject TEXT NOT NULL,
    from_json TEXT NOT NULL,
    to_json TEXT NOT NULL,
    cc_json TEXT NOT NULL,
    date_ms INTEGER NOT NULL,
    snippet TEXT NOT NULL,
    is_read INTEGER NOT NULL,
    is_flagged INTEGER NOT NULL,
    has_attachments INTEGER NOT NULL,
    UNIQUE (folder_id, remote_id)
);

CREATE INDEX idx_messages_folder_date ON messages (folder_id, date_ms DESC);
