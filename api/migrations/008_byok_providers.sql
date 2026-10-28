-- Migration 008: BYOK Providers & Custom API Endpoints (OmniRoute style)

CREATE TABLE IF NOT EXISTS user_byok_providers (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL,
    name TEXT NOT NULL,
    api_key TEXT NOT NULL DEFAULT '',
    base_url TEXT NOT NULL DEFAULT '',
    models TEXT NOT NULL DEFAULT '',
    protocol TEXT NOT NULL DEFAULT 'openai',
    enabled BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, provider_id)
);

CREATE INDEX IF NOT EXISTS idx_user_byok_user_id ON user_byok_providers(user_id);
