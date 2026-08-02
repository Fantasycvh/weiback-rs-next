-- P4-A: durable identity and recovery state for one-time legacy imports.
CREATE TABLE legacy_imports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_path TEXT NOT NULL,
    snapshot_fingerprint TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('rust_v1','python_v2')),
    status TEXT NOT NULL CHECK(status IN ('pending_publish','partial_recoverable','completed')),
    batch_dir TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    completed_at TEXT,
    UNIQUE(source_path, snapshot_fingerprint, source_kind)
);

CREATE INDEX idx_legacy_imports_batch_dir ON legacy_imports(batch_dir);
