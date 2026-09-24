-- Schema v2: drafts carry a sending state so a send can be undone (the
-- draft reappears with the error) and the sender address they use.

ALTER TABLE drafts ADD COLUMN state TEXT NOT NULL DEFAULT 'editing'
  CHECK (state IN ('editing', 'sending', 'failed'));
ALTER TABLE drafts ADD COLUMN last_error TEXT;
ALTER TABLE drafts ADD COLUMN rfc822_message_id TEXT;
