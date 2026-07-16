-- Provider-reported model usage, normalized per model invocation.

CREATE TABLE IF NOT EXISTS usage_events (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id    INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    idx                INTEGER NOT NULL,
    timestamp          INTEGER,
    provider           TEXT,
    model              TEXT,
    source_event_id    TEXT,
    api_calls          INTEGER NOT NULL DEFAULT 1,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    total_tokens       INTEGER NOT NULL DEFAULT 0,
    actual_cost_usd    REAL,
    estimated_cost_usd REAL,
    UNIQUE(conversation_id, idx)
);

CREATE INDEX IF NOT EXISTS idx_usage_conversation ON usage_events(conversation_id);
CREATE INDEX IF NOT EXISTS idx_usage_timestamp ON usage_events(timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_provider ON usage_events(provider);
CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_events(model);
CREATE INDEX IF NOT EXISTS idx_usage_source_event ON usage_events(source_event_id);
