CREATE EXTENSION IF NOT EXISTS citext;

CREATE TABLE IF NOT EXISTS users (
    id uuid PRIMARY KEY,
    email citext UNIQUE NOT NULL,
    password_hash text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id text PRIMARY KEY,
    redirect_uri_prefixes text[] NOT NULL
);

INSERT INTO oauth_clients (client_id, redirect_uri_prefixes)
VALUES ('varynth-cli', ARRAY['http://127.0.0.1:', 'http://[::1]:', 'http://localhost:'])
ON CONFLICT (client_id) DO NOTHING;

CREATE TABLE IF NOT EXISTS auth_codes (
    code text PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id),
    client_id text NOT NULL,
    redirect_uri text NOT NULL,
    code_challenge text NOT NULL,
    expires_at timestamptz NOT NULL,
    used_at timestamptz
);

CREATE TABLE IF NOT EXISTS refresh_tokens (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id),
    token_hash bytea UNIQUE NOT NULL,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz
);

CREATE TABLE IF NOT EXISTS models (
    id text PRIMARY KEY,
    display_name text NOT NULL,
    tier text NOT NULL CHECK (tier IN ('public', 'account')),
    upstream_kind text NOT NULL,
    upstream_model text NOT NULL,
    upstream_base_url text NOT NULL,
    enabled boolean NOT NULL DEFAULT false,
    sort_order integer NOT NULL DEFAULT 0
);

INSERT INTO models (id, display_name, tier, upstream_kind, upstream_model, upstream_base_url, enabled, sort_order)
VALUES
    ('varynth/public-1', 'Varynth Public 1', 'public', 'workers_ai', '', '', false, 1),
    ('varynth/public-2', 'Varynth Public 2', 'public', 'workers_ai', '', '', false, 2),
    ('varynth/public-3', 'Varynth Public 3', 'public', 'workers_ai', '', '', false, 3),
    ('varynth/public-4', 'Varynth Public 4', 'public', 'workers_ai', '', '', false, 4),
    ('varynth/public-5', 'Varynth Public 5', 'public', 'workers_ai', '', '', false, 5)
ON CONFLICT (id) DO NOTHING;

CREATE TABLE IF NOT EXISTS provider_catalog (
    id text PRIMARY KEY,
    display_name text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('openai', 'anthropic')),
    requires_account boolean NOT NULL DEFAULT true,
    sort_order integer NOT NULL DEFAULT 0
);

INSERT INTO provider_catalog (id, display_name, kind, requires_account, sort_order)
VALUES
    ('openai', 'OpenAI', 'openai', true, 10),
    ('anthropic', 'Anthropic', 'anthropic', true, 20),
    ('openrouter', 'OpenRouter', 'openai', true, 30)
ON CONFLICT (id) DO NOTHING;

CREATE TABLE IF NOT EXISTS provider_models (
    provider_id text NOT NULL REFERENCES provider_catalog (id),
    model_id text NOT NULL,
    display_name text NOT NULL,
    PRIMARY KEY (provider_id, model_id)
);

INSERT INTO provider_models (provider_id, model_id, display_name)
VALUES
    ('openai', 'gpt-4o-mini', 'GPT-4o mini'),
    ('anthropic', 'claude-sonnet-4', 'Claude Sonnet'),
    ('openrouter', 'auto', 'Auto')
ON CONFLICT (provider_id, model_id) DO NOTHING;
