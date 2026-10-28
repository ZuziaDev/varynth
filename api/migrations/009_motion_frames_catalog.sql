-- Migration 009: Register motion-frames and motion-frames-pro in skills catalog
INSERT INTO skills_catalog (id, name, description, tier, content)
VALUES
(
    'motion-frames',
    'Motion Frames',
    'Deterministic frame-by-frame HTML/CSS/Canvas/WebGL motion graphics & 60fps launch video generator with HeyGen Hyperframes architecture (Playwright + FFmpeg).',
    'free',
    $$---
name: motion-frames
description: Deterministic frame-by-frame HTML/CSS/Canvas/WebGL motion graphics & 60fps launch video generator with HeyGen Hyperframes architecture (Playwright + FFmpeg).
---

# motion-frames — Deterministic Web Motion Graphics Engine (HeyGen Hyperframes Architecture)

## Architecture
- Deterministic pure time representation: every visual frame `f` is a mathematical evaluation of `tSec = frame / fps`.
- HeyGen Hyperframes scene graph composition, easing curves (Newton-Raphson Bezier, analytical damped springs), and audio viseme timeline synchronization.
- Playwright virtual-clock frame stepper pipes lossless PNG buffers directly into FFmpeg `image2pipe`.
- Multiplex audio voiceovers synchronously with `-c:a aac -b:a 192k -shortest`.
$$
),
(
    'motion-frames-pro',
    'Motion Frames Pro',
    'Pro automated deterministic 60fps motion graphics studio with Flux AI hero assets, UI/UX Pro Max tokens, and programmatic code-driven avatar generation & animation rig.',
    'pro',
    $$---
name: motion-frames-pro
description: Pro automated deterministic 60fps motion graphics studio with Flux AI hero assets, UI/UX Pro Max tokens, and programmatic code-driven avatar generation & animation rig.
---

# motion-frames-pro — Pro AI Flux & Hyperframes Studio with UI/UX Pro Max & Procedural Avatar Rig

## Features
1. Synthesize photorealistic 3D hero assets via text-to-image `generate_image(prompt, output_path, size)`.
2. Programmatic code-driven 2D/3D procedural avatar rig on Canvas2D with 12 phonetic visemes, eye micro-saccades, and Poisson-distributed blinks.
3. UI/UX Pro Max design system tokens, deep obsidian void backgrounds, chromatic aberration shaders, and spring physics.
4. Deterministic 60fps headless capture streaming lossless PNGs into FFmpeg.
$$
)
ON CONFLICT (id) DO UPDATE SET
    name = EXCLUDED.name,
    description = EXCLUDED.description,
    tier = EXCLUDED.tier,
    content = EXCLUDED.content;
