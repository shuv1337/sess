-- Add embeddings table for semantic search

CREATE TABLE IF NOT EXISTS embeddings (
    conversation_id INTEGER PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    embedding       BLOB NOT NULL          -- f32 array as bytes
);