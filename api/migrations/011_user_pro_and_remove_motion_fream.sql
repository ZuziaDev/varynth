-- 011_user_pro_and_remove_motion_fream.sql
-- 1. Ensure all authenticated users are granted Pro privileges so they can use all Pro models and features.
UPDATE users SET plan = 'pro' WHERE plan = 'free';
ALTER TABLE users ALTER COLUMN plan SET DEFAULT 'pro';

-- 2. Remove Motion Fream and Motion Fream Pro skills from the catalog as requested.
DELETE FROM skills_catalog WHERE id IN ('motion-fream', 'motion-fream-pro', 'motion-frames', 'motion-frames-pro');
