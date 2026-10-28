-- Migration 006: Personal API Keys (vk_live_...) and Real-Time CLI Session Telemetry Sync

CREATE TABLE IF NOT EXISTS api_keys (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    key_prefix TEXT NOT NULL,
    key_hash BYTEA NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_api_keys_user_id ON api_keys(user_id);

CREATE TABLE IF NOT EXISTS cli_sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_key TEXT NOT NULL,
    title TEXT NOT NULL,
    model TEXT NOT NULL,
    workspace TEXT NOT NULL DEFAULT '',
    message_count INTEGER NOT NULL DEFAULT 0,
    preview TEXT NOT NULL DEFAULT '',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, session_key)
);

CREATE INDEX IF NOT EXISTS idx_cli_sessions_user_updated ON cli_sessions(user_id, updated_at DESC);
