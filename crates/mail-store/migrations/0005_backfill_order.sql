-- Backfill in listing order. Providers list newest first, so draining the
-- queue by insertion order fetches recent mail before old mail; the old
-- WITHOUT ROWID table ordered by id, which for Gmail meant oldest first.
CREATE TABLE backfill_queue_new (
  seq      INTEGER PRIMARY KEY AUTOINCREMENT,
  priority INTEGER NOT NULL,
  gmail_id TEXT NOT NULL UNIQUE
);
-- Existing rows: Gmail ids grow over time, so descending is newest first.
INSERT INTO backfill_queue_new (priority, gmail_id)
  SELECT priority, gmail_id FROM backfill_queue ORDER BY length(gmail_id) DESC, gmail_id DESC;
DROP TABLE backfill_queue;
ALTER TABLE backfill_queue_new RENAME TO backfill_queue;
CREATE INDEX backfill_by_priority ON backfill_queue (priority, seq);
