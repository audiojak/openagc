-- Schema v3: bytes Gmail returns inline with a message (small parts with
-- no attachment id), so they can be opened without another request.

ALTER TABLE attachments ADD COLUMN data BLOB;
