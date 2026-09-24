-- Schema v4: routines (spec §11.2, §11.6). One account per database, so no
-- account column. The definition is the routine's JSON; the prompt is
-- generated from it and fingerprinted to show "changed since published".

CREATE TABLE routines (
  id               INTEGER PRIMARY KEY,
  uuid             TEXT NOT NULL UNIQUE,
  name             TEXT NOT NULL,
  enabled          INTEGER NOT NULL DEFAULT 1,
  runner           TEXT NOT NULL,
  template_id      TEXT,
  template_version INTEGER,
  definition_json  TEXT NOT NULL,
  sync_fingerprint TEXT,
  cloud_url        TEXT,
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL
);

CREATE TABLE routine_runs (
  id           INTEGER PRIMARY KEY,
  routine_id   INTEGER NOT NULL REFERENCES routines (id) ON DELETE CASCADE,
  inferred     INTEGER NOT NULL,
  session_uuid TEXT,
  started_at   INTEGER NOT NULL,
  ended_at     INTEGER,
  status       TEXT NOT NULL,
  counts_json  TEXT NOT NULL DEFAULT '{}',
  report_text  TEXT,
  undone_at    INTEGER
);
CREATE INDEX routine_runs_by_routine ON routine_runs (routine_id, started_at);

CREATE TABLE routine_run_threads (
  run_id    INTEGER NOT NULL REFERENCES routine_runs (id) ON DELETE CASCADE,
  thread_id TEXT NOT NULL,
  bucket_id TEXT,
  PRIMARY KEY (run_id, thread_id)
) WITHOUT ROWID;
