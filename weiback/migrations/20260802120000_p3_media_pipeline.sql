-- P3-A: URL-addressed media assets, multi-owner references, and durable downloads.
ALTER TABLE media RENAME TO media_legacy;

CREATE TABLE media (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL UNIQUE,
    media_type TEXT NOT NULL CHECK(media_type IN ('picture','video','avatar','emoji')),
    local_path TEXT,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK(status IN ('pending','downloading','downloaded','failed')),
    retry_count INTEGER NOT NULL DEFAULT 0 CHECK(retry_count >= 0 AND retry_count <= 5),
    next_retry_at_epoch INTEGER,
    claim_token TEXT,
    claimed_at TEXT,
    last_error TEXT,
    content_type TEXT,
    content_length INTEGER CHECK(content_length IS NULL OR content_length >= 0),
    created_at TEXT NOT NULL,
    updated_at TEXT,
    CHECK((status = 'downloading') = (claim_token IS NOT NULL)),
    CHECK(status != 'downloaded' OR (local_path IS NOT NULL AND content_length IS NOT NULL))
);

CREATE TABLE media_references (
    media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    owner_type TEXT NOT NULL CHECK(owner_type IN ('post','user','comment')),
    owner_id INTEGER NOT NULL,
    definition TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    PRIMARY KEY(media_id, owner_type, owner_id, definition)
);

INSERT OR IGNORE INTO media(
    url,media_type,local_path,status,retry_count,last_error,created_at,updated_at,
    content_length,next_retry_at_epoch
)
SELECT url,
       CASE LOWER(media_type)
           WHEN 'image' THEN 'picture'
           WHEN 'picture' THEN 'picture'
           WHEN 'movie' THEN 'video'
           WHEN 'video' THEN 'video'
           WHEN 'avatar' THEN 'avatar'
           WHEN 'emoji' THEN 'emoji'
           ELSE 'picture'
       END,
       local_path,
       CASE WHEN status='downloaded' AND local_path IS NOT NULL THEN 'downloaded'
            WHEN status='failed' THEN 'failed' ELSE 'pending' END,
       MIN(MAX(retry_count,0),5),last_error,created_at,updated_at,
       CASE WHEN status='downloaded' AND local_path IS NOT NULL THEN 0 ELSE NULL END,
       CASE WHEN status='failed' AND retry_count < 5 THEN 0 ELSE NULL END
FROM media_legacy;

INSERT OR IGNORE INTO media(url,media_type,local_path,status,content_length,created_at)
SELECT url,'picture',
       CASE WHEN path IS NULL OR path='' THEN NULL
            WHEN REPLACE(path,'\','/') LIKE 'pictures/%' THEN REPLACE(path,'\','/')
            ELSE 'pictures/' || REPLACE(path,'\','/') END,
       CASE WHEN path IS NULL OR path='' THEN 'pending' ELSE 'downloaded' END,
       CASE WHEN path IS NULL OR path='' THEN NULL ELSE 0 END,
       '1970-01-01T00:00:00Z'
FROM picture WHERE url IS NOT NULL AND url != '';

UPDATE media
SET media_type='picture',
    local_path=(SELECT CASE WHEN p.path IS NULL OR p.path='' THEN NULL
                            WHEN REPLACE(p.path,'\','/') LIKE 'pictures/%' THEN REPLACE(p.path,'\','/')
                            ELSE 'pictures/' || REPLACE(p.path,'\','/') END
                FROM picture p WHERE p.url=media.url),
    status='downloaded',content_length=0,last_error=NULL,next_retry_at_epoch=NULL
WHERE status!='downloaded' AND EXISTS(
    SELECT 1 FROM picture p WHERE p.url=media.url AND p.path IS NOT NULL AND p.path!=''
);

INSERT OR IGNORE INTO media(url,media_type,local_path,status,content_length,created_at)
SELECT url,'video',
       CASE WHEN path IS NULL OR path='' THEN NULL
            WHEN REPLACE(path,'\','/') LIKE 'videos/%' THEN REPLACE(path,'\','/')
            ELSE 'videos/' || REPLACE(path,'\','/') END,
       CASE WHEN path IS NULL OR path='' THEN 'pending' ELSE 'downloaded' END,
       CASE WHEN path IS NULL OR path='' THEN NULL ELSE 0 END,
       '1970-01-01T00:00:00Z'
FROM video WHERE url IS NOT NULL AND url != '';

-- One URL has one asset. For conflicting old rows video wins: validating video bytes as a
-- picture makes the stored asset unusable. Owner references remain distinct below.
UPDATE media
SET media_type='video',
    local_path=(SELECT CASE WHEN v.path IS NULL OR v.path='' THEN NULL
                            WHEN REPLACE(v.path,'\','/') LIKE 'videos/%' THEN REPLACE(v.path,'\','/')
                            ELSE 'videos/' || REPLACE(v.path,'\','/') END
                FROM video v WHERE v.url=media.url),
    status='downloaded',content_length=0,last_error=NULL,next_retry_at_epoch=NULL
WHERE EXISTS(
    SELECT 1 FROM video v WHERE v.url=media.url AND v.path IS NOT NULL AND v.path!=''
);

INSERT OR IGNORE INTO media_references(media_id,owner_type,owner_id,definition,created_at)
SELECT m.id,p.owner_type,p.owner_id,'',p.created_at
FROM media_legacy p JOIN media m ON m.url=p.url
WHERE p.owner_type IN ('post','user','comment') AND p.owner_id IS NOT NULL;

INSERT OR IGNORE INTO media_references(media_id,owner_type,owner_id,definition,created_at)
SELECT m.id,'post',p.post_id,COALESCE(p.definition,''),'1970-01-01T00:00:00Z'
FROM picture p JOIN media m ON m.url=p.url WHERE p.post_id IS NOT NULL;

INSERT OR IGNORE INTO media_references(media_id,owner_type,owner_id,definition,created_at)
SELECT m.id,'user',p.user_id,COALESCE(p.definition,''),'1970-01-01T00:00:00Z'
FROM picture p JOIN media m ON m.url=p.url WHERE p.user_id IS NOT NULL;

INSERT OR IGNORE INTO media_references(media_id,owner_type,owner_id,definition,created_at)
SELECT m.id,'post',v.post_id,'','1970-01-01T00:00:00Z'
FROM video v JOIN media m ON m.url=v.url WHERE v.post_id IS NOT NULL;

-- A concrete picture definition supersedes the legacy owner-only reference.
DELETE FROM media_references AS empty_reference
WHERE empty_reference.definition='' AND EXISTS(
    SELECT 1 FROM media_references AS concrete_reference
    WHERE concrete_reference.media_id=empty_reference.media_id
      AND concrete_reference.owner_type=empty_reference.owner_type
      AND concrete_reference.owner_id=empty_reference.owner_id
      AND concrete_reference.definition!=''
);

CREATE INDEX idx_media_status_retry ON media(status,next_retry_at_epoch,id);
CREATE INDEX idx_media_references_owner ON media_references(owner_type,owner_id);
