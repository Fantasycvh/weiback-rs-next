-- Imported assets cannot be claimed until their post-commit manifest is published.
ALTER TABLE media ADD COLUMN import_hold INTEGER NOT NULL DEFAULT 0 CHECK(import_hold IN (0,1));
ALTER TABLE media ADD COLUMN import_batch TEXT;

CREATE INDEX idx_media_claimable ON media(import_hold,status,next_retry_at_epoch,id);
