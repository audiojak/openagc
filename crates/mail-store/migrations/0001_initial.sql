-- OpenAGC mail store, schema v1 (spec §6.2).
-- One database per account. Provider ids (Gmail's) are unique text columns;
-- integer rowids are local and never leave the store.
--
-- Denormalized columns (threads.*, thread_labels, messages_fts, contacts)
-- are maintained by the store's write API in the same transaction as the
-- change they reflect; nothing else writes this database.

CREATE TABLE account (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  uuid         TEXT NOT NULL,
  email        TEXT NOT NULL,
  display_name TEXT,
  created_at   INTEGER NOT NULL
);

-- Key/value sync bookkeeping: history id, bootstrap progress.
CREATE TABLE sync_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE labels (
  id          INTEGER PRIMARY KEY,
  gmail_id    TEXT NOT NULL UNIQUE,
  name        TEXT NOT NULL,
  -- 'virtual' labels are local (e.g. @archive) and never sent to Gmail.
  kind        TEXT NOT NULL CHECK (kind IN ('system', 'user', 'virtual')),
  color_bg    TEXT,
  color_fg    TEXT,
  visible     INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE threads (
  id                INTEGER PRIMARY KEY,
  gmail_id          TEXT NOT NULL UNIQUE,
  subject           TEXT NOT NULL DEFAULT '',
  snippet           TEXT NOT NULL DEFAULT '',
  first_message_at  INTEGER NOT NULL DEFAULT 0,
  last_message_at   INTEGER NOT NULL DEFAULT 0,
  message_count     INTEGER NOT NULL DEFAULT 0,
  unread_count      INTEGER NOT NULL DEFAULT 0,
  has_attachments   INTEGER NOT NULL DEFAULT 0,
  is_starred        INTEGER NOT NULL DEFAULT 0,
  -- [{name,email}] distinct senders, oldest first: renders the list row.
  participants_json TEXT NOT NULL DEFAULT '[]',
  -- Provider label ids on any message of the thread.
  label_ids_json    TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX threads_by_last ON threads (last_message_at DESC, id DESC);

CREATE TABLE messages (
  id                INTEGER PRIMARY KEY,
  thread_id         INTEGER NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
  gmail_id          TEXT NOT NULL UNIQUE,
  rfc822_message_id TEXT,
  in_reply_to       TEXT,
  references_json   TEXT NOT NULL DEFAULT '[]',
  from_name         TEXT,
  from_email        TEXT,
  subject           TEXT NOT NULL DEFAULT '',
  snippet           TEXT NOT NULL DEFAULT '',
  date              INTEGER NOT NULL,
  internal_date     INTEGER NOT NULL,
  size_estimate     INTEGER NOT NULL DEFAULT 0,
  body_state        TEXT NOT NULL DEFAULT 'metadata' CHECK (body_state IN ('metadata', 'full')),
  is_read           INTEGER NOT NULL DEFAULT 0,
  is_starred        INTEGER NOT NULL DEFAULT 0,
  is_draft          INTEGER NOT NULL DEFAULT 0,
  is_sent_by_me     INTEGER NOT NULL DEFAULT 0,
  has_attachments   INTEGER NOT NULL DEFAULT 0,
  headers_json      TEXT
);
CREATE INDEX messages_by_thread ON messages (thread_id, internal_date);
CREATE INDEX messages_by_rfc822 ON messages (rfc822_message_id);

CREATE TABLE message_labels (
  message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
  label_id   INTEGER NOT NULL REFERENCES labels (id) ON DELETE CASCADE,
  PRIMARY KEY (message_id, label_id)
) WITHOUT ROWID;
CREATE INDEX message_labels_by_label ON message_labels (label_id, message_id);

-- What the thread list reads: one row per (label, thread), ordered for a
-- keyset scan, so `WHERE label_id = ? ORDER BY last_message_at DESC` never
-- touches another table to decide order.
CREATE TABLE thread_labels (
  label_id        INTEGER NOT NULL REFERENCES labels (id) ON DELETE CASCADE,
  last_message_at INTEGER NOT NULL,
  thread_id       INTEGER NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
  PRIMARY KEY (label_id, last_message_at, thread_id)
) WITHOUT ROWID;
CREATE INDEX thread_labels_by_thread ON thread_labels (thread_id);

-- Per-label thread counts for sidebar badges, updated by deltas when
-- threads are recomputed, so no COUNT(*) over large mailboxes.
CREATE TABLE label_stats (
  label_id            INTEGER PRIMARY KEY REFERENCES labels (id) ON DELETE CASCADE,
  thread_count        INTEGER NOT NULL DEFAULT 0,
  unread_thread_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE participants (
  message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
  role       TEXT NOT NULL CHECK (role IN ('from', 'to', 'cc', 'bcc', 'reply_to')),
  position   INTEGER NOT NULL,
  name       TEXT,
  email      TEXT NOT NULL,
  PRIMARY KEY (message_id, role, position)
) WITHOUT ROWID;
CREATE INDEX participants_by_email ON participants (email COLLATE NOCASE);

CREATE TABLE bodies (
  message_id        INTEGER PRIMARY KEY REFERENCES messages (id) ON DELETE CASCADE,
  text_plain        TEXT,
  html_sanitized    TEXT,
  html_original     BLOB,
  has_remote_images INTEGER NOT NULL DEFAULT 0,
  sanitizer_version INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE attachments (
  id                  INTEGER PRIMARY KEY,
  message_id          INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
  part_id             TEXT,
  gmail_attachment_id TEXT,
  filename            TEXT NOT NULL DEFAULT '',
  mime_type           TEXT NOT NULL DEFAULT 'application/octet-stream',
  size                INTEGER NOT NULL DEFAULT 0,
  content_id          TEXT,
  is_inline           INTEGER NOT NULL DEFAULT 0,
  local_path          TEXT
);
CREATE INDEX attachments_by_message ON attachments (message_id);

-- Full-text index over messages. Contentless with contentless_delete
-- (SQLite >= 3.43): rows are removed by rowid, so there are no old values
-- to replay and no second copy of bodies. rowid = messages.id.
CREATE VIRTUAL TABLE messages_fts USING fts5 (
  subject, from_text, to_text, body, attachment_names,
  content = '', contentless_delete = 1,
  tokenize = 'unicode61 remove_diacritics 2'
);

-- Everyone the user has corresponded with, for recipient autocomplete
-- (frecency) and partial-address search.
CREATE TABLE contacts (
  id             INTEGER PRIMARY KEY,
  email          TEXT NOT NULL UNIQUE COLLATE NOCASE,
  name           TEXT,
  sent_count     INTEGER NOT NULL DEFAULT 0,
  received_count INTEGER NOT NULL DEFAULT 0,
  last_seen      INTEGER NOT NULL DEFAULT 0
);
CREATE VIRTUAL TABLE contacts_fts USING fts5 (
  name, email, content = 'contacts', content_rowid = 'id', tokenize = 'trigram'
);
CREATE TRIGGER contacts_ai AFTER INSERT ON contacts BEGIN
  INSERT INTO contacts_fts (rowid, name, email) VALUES (new.id, new.name, new.email);
END;
CREATE TRIGGER contacts_ad AFTER DELETE ON contacts BEGIN
  INSERT INTO contacts_fts (contacts_fts, rowid, name, email) VALUES ('delete', old.id, old.name, old.email);
END;
CREATE TRIGGER contacts_au AFTER UPDATE OF name, email ON contacts BEGIN
  INSERT INTO contacts_fts (contacts_fts, rowid, name, email) VALUES ('delete', old.id, old.name, old.email);
  INSERT INTO contacts_fts (rowid, name, email) VALUES (new.id, new.name, new.email);
END;

-- Messages waiting for a full fetch, drained in priority order (spec §7.4).
CREATE TABLE backfill_queue (
  priority INTEGER NOT NULL,
  gmail_id TEXT NOT NULL,
  PRIMARY KEY (priority, gmail_id)
) WITHOUT ROWID;
CREATE UNIQUE INDEX backfill_by_gmail_id ON backfill_queue (gmail_id);

CREATE TABLE drafts (
  id                     INTEGER PRIMARY KEY,
  gmail_draft_id         TEXT UNIQUE,
  thread_id              TEXT,
  in_reply_to_message_id TEXT,
  to_json                TEXT NOT NULL DEFAULT '[]',
  cc_json                TEXT NOT NULL DEFAULT '[]',
  bcc_json               TEXT NOT NULL DEFAULT '[]',
  subject                TEXT NOT NULL DEFAULT '',
  body_html              TEXT NOT NULL DEFAULT '',
  body_text              TEXT NOT NULL DEFAULT '',
  attachments_json       TEXT NOT NULL DEFAULT '[]',
  updated_at             INTEGER NOT NULL,
  dirty                  INTEGER NOT NULL DEFAULT 1
);

-- Local mutations waiting to be applied to the provider (spec §7.4).
CREATE TABLE outbox (
  id              INTEGER PRIMARY KEY,
  kind            TEXT NOT NULL,
  payload_json    TEXT NOT NULL,
  created_at      INTEGER NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER,
  state           TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'in_flight', 'failed')),
  last_error      TEXT
);

CREATE TABLE agent_sessions (
  id           INTEGER PRIMARY KEY,
  uuid         TEXT NOT NULL UNIQUE,
  provider     TEXT NOT NULL,
  external_id  TEXT,
  state        TEXT NOT NULL,
  started_at   INTEGER NOT NULL,
  ended_at     INTEGER,
  prompt_count INTEGER NOT NULL DEFAULT 0,
  cost_usd     REAL
);

CREATE TABLE agent_actions (
  id             INTEGER PRIMARY KEY,
  session_id     INTEGER NOT NULL REFERENCES agent_sessions (id) ON DELETE CASCADE,
  tool           TEXT NOT NULL,
  args_json      TEXT NOT NULL,
  risk           TEXT NOT NULL CHECK (risk IN ('read_only', 'reversible', 'external')),
  state          TEXT NOT NULL,
  result_summary TEXT,
  created_at     INTEGER NOT NULL,
  resolved_at    INTEGER
);
CREATE INDEX agent_actions_by_session ON agent_actions (session_id, id);

CREATE TABLE agent_transcript (
  session_id   INTEGER NOT NULL REFERENCES agent_sessions (id) ON DELETE CASCADE,
  seq          INTEGER NOT NULL,
  role         TEXT NOT NULL,
  content_json TEXT NOT NULL,
  PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;

-- The virtual Archive label (spec §5): threads with no INBOX label that are
-- not wholly spam or trash. Maintained with every other thread_labels row.
INSERT INTO labels (gmail_id, name, kind, visible) VALUES ('@archive', 'Archive', 'virtual', 1);
