CREATE TABLE IF NOT EXISTS subscription_events (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id),
    from_plan text NOT NULL CHECK (from_plan IN ('free', 'pro')),
    to_plan text NOT NULL CHECK (to_plan IN ('free', 'pro')),
    created_at timestamptz NOT NULL DEFAULT now(),
    source text NOT NULL CHECK (source IN ('stub', 'provider', 'admin'))
);

CREATE INDEX IF NOT EXISTS subscription_events_user_created
    ON subscription_events (user_id, created_at DESC);
