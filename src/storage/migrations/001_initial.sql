-- Initial schema

-- Metadata table
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Conversations table
CREATE TABLE IF NOT EXISTS conversations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    agent       TEXT NOT NULL,           -- 'claude_code', 'codex', 'hermes', 'opencode', 'pi_agent'
    external_id TEXT,
    title       TEXT,
    workspace   TEXT,
    source_path TEXT NOT NULL UNIQUE,    -- canonical source key
    started_at  INTEGER,                 -- Unix millis
    ended_at    INTEGER,                 -- Unix millis
    indexed_at  INTEGER NOT NULL,        -- Unix millis when we last indexed this conversation
    source_mtime_max INTEGER NOT NULL,   -- max mtime across all source_files
    source_fingerprint TEXT NOT NULL     -- blake3 hash
);

CREATE INDEX IF NOT EXISTS idx_conv_agent ON conversations(agent);
CREATE INDEX IF NOT EXISTS idx_conv_workspace ON conversations(workspace);
CREATE INDEX IF NOT EXISTS idx_conv_started ON conversations(started_at);
CREATE INDEX IF NOT EXISTS idx_conv_fingerprint ON conversations(source_fingerprint);

-- Messages table
CREATE TABLE IF NOT EXISTS messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    idx             INTEGER NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    timestamp       INTEGER,
    model           TEXT,
    content_hash    TEXT NOT NULL       -- blake3 hash for dedup
);

CREATE INDEX IF NOT EXISTS idx_msg_conv ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_msg_hash ON messages(content_hash);
