-- Migration 0002 (contracts-v1): vector index. Requires the sqlite-vec extension to be loaded first.
-- Embedding dimension is fixed at 384 (bge-small-en-v1.5 class models). Changing it needs a new migration.
CREATE VIRTUAL TABLE vec_chunks USING vec0(embedding float[384]);
UPDATE schema_version SET version = 2;
