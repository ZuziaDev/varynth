ALTER TABLE users ADD COLUMN IF NOT EXISTS plan text NOT NULL DEFAULT 'free';

DO $$
BEGIN
    ALTER TABLE users ADD CONSTRAINT users_plan_check CHECK (plan IN ('free', 'pro'));
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

CREATE TABLE IF NOT EXISTS usage_counters (
    subject text NOT NULL,
    kind text NOT NULL,
    bucket text NOT NULL,
    count integer NOT NULL DEFAULT 0,
    PRIMARY KEY (subject, kind, bucket)
);
