-- Usage provenance, aggregate timing, transcript hierarchy, and token-shape
-- semantics. Raw provider/model columns remain unchanged.

ALTER TABLE conversations ADD COLUMN logical_session_id TEXT;
ALTER TABLE conversations ADD COLUMN parent_external_id TEXT;
ALTER TABLE conversations ADD COLUMN record_kind TEXT NOT NULL DEFAULT 'top_level';
ALTER TABLE conversations ADD COLUMN is_synthetic INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_conv_logical_session ON conversations(agent, logical_session_id);
CREATE INDEX IF NOT EXISTS idx_conv_record_kind ON conversations(record_kind);
CREATE INDEX IF NOT EXISTS idx_conv_synthetic ON conversations(is_synthetic);

ALTER TABLE usage_events ADD COLUMN interval_start INTEGER;
ALTER TABLE usage_events ADD COLUMN interval_end INTEGER;
ALTER TABLE usage_events ADD COLUMN usage_grain TEXT NOT NULL DEFAULT 'event';
ALTER TABLE usage_events ADD COLUMN provider_family TEXT;
ALTER TABLE usage_events ADD COLUMN provider_inference_source TEXT;
ALTER TABLE usage_events ADD COLUMN provider_inference_confidence TEXT;
ALTER TABLE usage_events ADD COLUMN model_family TEXT;
ALTER TABLE usage_events ADD COLUMN model_variant TEXT;
ALTER TABLE usage_events ADD COLUMN task TEXT;
ALTER TABLE usage_events ADD COLUMN billing_base_url TEXT;
ALTER TABLE usage_events ADD COLUMN billing_mode TEXT;
ALTER TABLE usage_events ADD COLUMN request_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_events ADD COLUMN reported_total_tokens INTEGER;
ALTER TABLE usage_events ADD COLUMN component_total_tokens INTEGER;
ALTER TABLE usage_events ADD COLUMN token_semantics TEXT;
ALTER TABLE usage_events ADD COLUMN cost_status TEXT;
ALTER TABLE usage_events ADD COLUMN cost_source TEXT;
ALTER TABLE usage_events ADD COLUMN cost_currency TEXT;
ALTER TABLE usage_events ADD COLUMN pricing_version TEXT;

-- Keep an upgraded database useful before the parser-revision migration scan.
UPDATE conversations
SET external_id = NULLIF(external_id, ''),
    workspace = NULLIF(workspace, ''),
    started_at = NULLIF(started_at, 0),
    ended_at = NULLIF(ended_at, 0),
    logical_session_id = NULLIF(external_id, ''),
    record_kind = 'top_level'
WHERE logical_session_id IS NULL;

UPDATE messages
SET timestamp = NULLIF(timestamp, 0),
    model = NULLIF(model, '');

UPDATE usage_events
SET request_attempts = api_calls,
    component_total_tokens = min(
        input_tokens + output_tokens + cache_read_tokens + cache_write_tokens,
        9223372036854775807
    ),
    cost_status = CASE
        WHEN actual_cost_usd > 0
             AND actual_cost_usd <= 1.7976931348623157e308 THEN 'reported_actual'
        WHEN estimated_cost_usd > 0
             AND estimated_cost_usd <= 1.7976931348623157e308 THEN 'source_estimated'
        ELSE 'unknown'
    END,
    cost_currency = CASE
        WHEN (actual_cost_usd > 0
              AND actual_cost_usd <= 1.7976931348623157e308)
          OR (estimated_cost_usd > 0
              AND estimated_cost_usd <= 1.7976931348623157e308) THEN 'USD'
        ELSE NULL
    END;

CREATE INDEX IF NOT EXISTS idx_usage_interval ON usage_events(interval_start, interval_end);
CREATE INDEX IF NOT EXISTS idx_usage_grain ON usage_events(usage_grain);
CREATE INDEX IF NOT EXISTS idx_usage_provider_family ON usage_events(provider_family);
CREATE INDEX IF NOT EXISTS idx_usage_model_family ON usage_events(model_family);
CREATE INDEX IF NOT EXISTS idx_usage_variant ON usage_events(model_variant);
CREATE INDEX IF NOT EXISTS idx_usage_cost_status ON usage_events(cost_status);
