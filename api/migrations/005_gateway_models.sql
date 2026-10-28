-- Seed all 61 real models from the configured upstream gateways into both
-- the public models table (`models`) and the provider catalog (`provider_catalog` / `provider_models`).

INSERT INTO models (id, display_name, tier, upstream_kind, upstream_model, upstream_base_url, enabled, sort_order)
VALUES
    -- Flagship & Reasoning Chat Models
    ('varynth/gpt-oss-120b', 'OpenAI GPT-OSS 120B (Reasoning · Tools)', 'public', 'worker_gateway', 'gpt-oss-120b', '', true, 10),
    ('varynth/kimi-k2.6', 'Moonshot Kimi K2.6 (Vision · Reasoning · Tools)', 'public', 'worker_gateway', 'kimi-k2.6', '', true, 11),
    ('varynth/qwen3.8-27b', 'Qwen 3.8 27B (Vision · Reasoning · Tools)', 'public', 'worker_gateway', 'qwen3.8-27b', '', true, 12),
    ('varynth/nemotron-3-120b-a12b', 'NVIDIA Nemotron 3 120B A12B (Reasoning · Tools)', 'public', 'worker_gateway', 'nemotron-3-120b-a12b', '', true, 13),
    ('varynth/qwen3-30b-a3b-fp8', 'Qwen 3 30B A3B FP8 (Reasoning · Tools)', 'public', 'worker_gateway', 'qwen3-30b-a3b-fp8', '', true, 14),
    ('varynth/qwen2.5-coder-32b-instruct', 'Qwen 2.5 Coder 32B Instruct (Coding)', 'public', 'worker_gateway', 'qwen2.5-coder-32b-instruct', '', true, 15),
    ('varynth/deepseek-r1-distill-qwen-32b', 'DeepSeek R1 Distill Qwen 32B (Reasoning)', 'public', 'worker_gateway', 'deepseek-r1-distill-qwen-32b', '', true, 16),
    ('varynth/qwq-32b', 'Qwen QwQ 32B (Reasoning)', 'public', 'worker_gateway', 'qwq-32b', '', true, 17),
    ('varynth/llama-4-scout-17b-16e-instruct', 'Meta Llama 4 Scout 17B 16E (Vision · Tools)', 'public', 'worker_gateway', 'llama-4-scout-17b-16e-instruct', '', true, 18),
    ('varynth/llama-3.3-70b-instruct-fp8-fast', 'Meta Llama 3.3 70B Instruct FP8 Fast (Tools)', 'public', 'worker_gateway', 'llama-3.3-70b-instruct-fp8-fast', '', true, 19),
    ('varynth/gemma-4-26b-a4b-it', 'Google Gemma 4 26B A4B IT (Vision · Reasoning · Tools)', 'public', 'worker_gateway', 'gemma-4-26b-a4b-it', '', true, 20),
    ('varynth/glm-4.7-flash', 'Z.AI GLM 4.7 Flash (Reasoning · Tools)', 'public', 'worker_gateway', 'glm-4.7-flash', '', true, 21),
    ('varynth/mistral-small-3.1-24b-instruct', 'Mistral Small 3.1 24B Instruct (Vision · Tools)', 'public', 'worker_gateway', 'mistral-small-3.1-24b-instruct', '', true, 22),
    ('varynth/gpt-oss-20b', 'OpenAI GPT-OSS 20B (Fast · Reasoning · Tools)', 'public', 'worker_gateway', 'gpt-oss-20b', '', true, 23),
    ('varynth/gemma-sea-lion-v4-27b-it', 'AI Singapore Gemma SEA-LION v4 27B IT', 'public', 'worker_gateway', 'gemma-sea-lion-v4-27b-it', '', true, 24),
    ('varynth/llama-3.2-11b-vision-instruct', 'Meta Llama 3.2 11B Vision Instruct', 'public', 'worker_gateway', 'llama-3.2-11b-vision-instruct', '', true, 25),
    ('varynth/llama-3.1-8b-instruct-fast', 'Meta Llama 3.1 8B Instruct Fast', 'public', 'worker_gateway', 'llama-3.1-8b-instruct-fast', '', true, 26),
    ('varynth/llama-3.1-8b-instruct-fp8', 'Meta Llama 3.1 8B Instruct FP8', 'public', 'worker_gateway', 'llama-3.1-8b-instruct-fp8', '', true, 27),
    ('varynth/granite-4.0-h-micro', 'IBM Granite 4.0 H Micro (Fast · Tools)', 'public', 'worker_gateway', 'granite-4.0-h-micro', '', true, 28),
    ('varynth/llama-guard-3-8b', 'Meta Llama Guard 3 8B (Safety & Moderation)', 'public', 'worker_gateway', 'llama-guard-3-8b', '', true, 29),
    ('varynth/llama-3.2-3b-instruct', 'Meta Llama 3.2 3B Instruct', 'public', 'worker_gateway', 'llama-3.2-3b-instruct', '', true, 30),
    ('varynth/llama-3.2-1b-instruct', 'Meta Llama 3.2 1B Instruct', 'public', 'worker_gateway', 'llama-3.2-1b-instruct', '', true, 31),
    ('varynth/mistral-7b-instruct-v0.2-lora', 'Mistral 7B Instruct v0.2 LoRA', 'public', 'worker_gateway', 'mistral-7b-instruct-v0.2-lora', '', true, 32),
    ('varynth/gemma-7b-it-lora', 'Google Gemma 7B IT LoRA', 'public', 'worker_gateway', 'gemma-7b-it-lora', '', true, 33),
    ('varynth/gemma-2b-it-lora', 'Google Gemma 2B IT LoRA', 'public', 'worker_gateway', 'gemma-2b-it-lora', '', true, 34),
    ('varynth/llama-2-7b-chat-hf-lora', 'Meta Llama 2 7B Chat LoRA', 'public', 'worker_gateway', 'llama-2-7b-chat-hf-lora', '', true, 35),

    -- Image Generation & Vision Analysis Models
    ('varynth/flux-2-dev', 'Black Forest Labs Flux 2 Dev (Image Generation & Edit)', 'public', 'worker_gateway', 'flux-2-dev', '', true, 40),
    ('varynth/flux-2-klein-9b', 'Black Forest Labs Flux 2 Klein 9B (Image Generation)', 'public', 'worker_gateway', 'flux-2-klein-9b', '', true, 41),
    ('varynth/flux-2-klein-4b', 'Black Forest Labs Flux 2 Klein 4B (Image Generation)', 'public', 'worker_gateway', 'flux-2-klein-4b', '', true, 42),
    ('varynth/flux-1-schnell', 'Black Forest Labs Flux 1 Schnell (Fast Image Synthesis)', 'public', 'worker_gateway', 'flux-1-schnell', '', true, 43),
    ('varynth/phoenix-1.0', 'Leonardo Phoenix 1.0 (Image Generation)', 'public', 'worker_gateway', 'phoenix-1.0', '', true, 44),
    ('varynth/lucid-origin', 'Leonardo Lucid Origin (Image Generation)', 'public', 'worker_gateway', 'lucid-origin', '', true, 45),
    ('varynth/stable-diffusion-xl-lightning', 'ByteDance SDXL Lightning (Image Generation)', 'public', 'worker_gateway', 'stable-diffusion-xl-lightning', '', true, 46),
    ('varynth/stable-diffusion-xl-base-1.0', 'Stability AI SDXL Base 1.0 (Image Generation)', 'public', 'worker_gateway', 'stable-diffusion-xl-base-1.0', '', true, 47),
    ('varynth/stable-diffusion-v1-5-inpainting', 'RunwayML SD v1.5 Inpainting', 'public', 'worker_gateway', 'stable-diffusion-v1-5-inpainting', '', true, 48),
    ('varynth/stable-diffusion-v1-5-img2img', 'RunwayML SD v1.5 Image-to-Image', 'public', 'worker_gateway', 'stable-diffusion-v1-5-img2img', '', true, 49),
    ('varynth/dreamshaper-8-lcm', 'Lykon DreamShaper 8 LCM (Image Generation)', 'public', 'worker_gateway', 'dreamshaper-8-lcm', '', true, 50),
    ('varynth/moondream3.1-9B-A2B', 'Moondream 3.1 9B A2B (Image-to-Text Vision)', 'public', 'worker_gateway', 'moondream3.1-9B-A2B', '', true, 51),
    ('varynth/llava-1.5-7b-hf', 'LLaVA 1.5 7B (Image-to-Text Vision)', 'public', 'worker_gateway', 'llava-1.5-7b-hf', '', true, 52),
    ('varynth/resnet-50', 'Microsoft ResNet-50 (Image Classification)', 'public', 'worker_gateway', 'resnet-50', '', true, 53),

    -- Audio, ASR, TTS & Turn Detection Models
    ('varynth/whisper-large-v3-turbo', 'OpenAI Whisper Large v3 Turbo (Speech-to-Text)', 'public', 'worker_gateway', 'whisper-large-v3-turbo', '', true, 60),
    ('varynth/whisper-tiny-en', 'OpenAI Whisper Tiny EN (Speech-to-Text)', 'public', 'worker_gateway', 'whisper-tiny-en', '', true, 61),
    ('varynth/whisper', 'OpenAI Whisper Base (Speech-to-Text)', 'public', 'worker_gateway', 'whisper', '', true, 62),
    ('varynth/nova-3', 'Deepgram Nova 3 (Realtime ASR)', 'public', 'worker_gateway', 'nova-3', '', true, 63),
    ('varynth/deepgram-flux', 'Deepgram Flux (Realtime Conversational ASR)', 'public', 'worker_gateway', 'deepgram-flux', '', true, 64),
    ('varynth/aura-2-en', 'Deepgram Aura 2 English (Realtime TTS)', 'public', 'worker_gateway', 'aura-2-en', '', true, 65),
    ('varynth/aura-2-es', 'Deepgram Aura 2 Spanish (Realtime TTS)', 'public', 'worker_gateway', 'aura-2-es', '', true, 66),
    ('varynth/aura-1', 'Deepgram Aura 1 (Realtime TTS)', 'public', 'worker_gateway', 'aura-1', '', true, 67),
    ('varynth/melotts', 'MyShell MeloTTS (Text-to-Speech)', 'public', 'worker_gateway', 'melotts', '', true, 68),
    ('varynth/smart-turn-v2', 'Pipecat Smart Turn v2 (Audio Turn Detection)', 'public', 'worker_gateway', 'smart-turn-v2', '', true, 69),

    -- Embeddings, Reranking, Translation & Classification Models
    ('varynth/bge-m3', 'BAAI BGE-M3 (Multilingual Embeddings)', 'public', 'worker_gateway', 'bge-m3', '', true, 80),
    ('varynth/bge-large-en-v1.5', 'BAAI BGE Large EN v1.5 (Embeddings)', 'public', 'worker_gateway', 'bge-large-en-v1.5', '', true, 81),
    ('varynth/bge-base-en-v1.5', 'BAAI BGE Base EN v1.5 (Embeddings)', 'public', 'worker_gateway', 'bge-base-en-v1.5', '', true, 82),
    ('varynth/bge-small-en-v1.5', 'BAAI BGE Small EN v1.5 (Embeddings)', 'public', 'worker_gateway', 'bge-small-en-v1.5', '', true, 83),
    ('varynth/bge-reranker-base', 'BAAI BGE Reranker Base (Rerank)', 'public', 'worker_gateway', 'bge-reranker-base', '', true, 84),
    ('varynth/qwen3-embedding-0.6b', 'Qwen 3 Embedding 0.6B', 'public', 'worker_gateway', 'qwen3-embedding-0.6b', '', true, 85),
    ('varynth/plamo-embedding-1b', 'PFNet PLaMo Embedding 1B', 'public', 'worker_gateway', 'plamo-embedding-1b', '', true, 86),
    ('varynth/embeddinggemma-300m', 'Google EmbeddingGemma 300M', 'public', 'worker_gateway', 'embeddinggemma-300m', '', true, 87),
    ('varynth/m2m100-1.2b', 'Meta M2M100 1.2B (Multilingual Translation)', 'public', 'worker_gateway', 'm2m100-1.2b', '', true, 88),
    ('varynth/indictrans2-en-indic-1b', 'AI4Bharat IndicTrans2 1B (Translation)', 'public', 'worker_gateway', 'indictrans2-en-indic-1b', '', true, 89),
    ('varynth/distilbert-sst-2-int8', 'HuggingFace DistilBERT SST-2 Int8 (Classification)', 'public', 'worker_gateway', 'distilbert-sst-2-int8', '', true, 90)
ON CONFLICT (id) DO UPDATE SET
    display_name = EXCLUDED.display_name,
    upstream_kind = EXCLUDED.upstream_kind,
    upstream_model = EXCLUDED.upstream_model,
    enabled = EXCLUDED.enabled,
    sort_order = EXCLUDED.sort_order;

-- Seed provider_catalog for account catalog grouping
INSERT INTO provider_catalog (id, display_name, kind, requires_account, sort_order)
VALUES
    ('openai', 'OpenAI', 'openai', true, 10),
    ('qwen', 'Qwen (Alibaba Cloud)', 'openai', true, 12),
    ('deepseek', 'DeepSeek AI', 'openai', true, 14),
    ('moonshotai', 'Moonshot AI (Kimi)', 'openai', true, 16),
    ('nvidia', 'NVIDIA Nemotron', 'openai', true, 18),
    ('meta', 'Meta Llama', 'openai', true, 20),
    ('google', 'Google Gemma', 'openai', true, 22),
    ('mistralai', 'Mistral AI', 'openai', true, 24),
    ('zai', 'Z.AI (GLM)', 'openai', true, 26),
    ('ibm', 'IBM Granite', 'openai', true, 28),
    ('black-forest-labs', 'Black Forest Labs (Flux)', 'openai', true, 30),
    ('leonardo', 'Leonardo AI', 'openai', true, 32),
    ('stabilityai', 'Stability & Diffusion', 'openai', true, 34),
    ('deepgram', 'Deepgram & Speech Audio', 'openai', true, 36),
    ('baai', 'BAAI Embeddings & Rerank', 'openai', true, 38),
    ('multimodal', 'Vision, Translation & Classification', 'openai', true, 40),
    ('anthropic', 'Anthropic', 'anthropic', true, 50),
    ('openrouter', 'OpenRouter', 'openai', true, 60)
ON CONFLICT (id) DO UPDATE SET
    display_name = EXCLUDED.display_name,
    sort_order = EXCLUDED.sort_order;

INSERT INTO provider_models (provider_id, model_id, display_name)
VALUES
    -- OpenAI
    ('openai', 'gpt-oss-120b', 'GPT-OSS 120B (Reasoning · Tools)'),
    ('openai', 'gpt-oss-20b', 'GPT-OSS 20B (Fast · Reasoning · Tools)'),
    ('openai', 'whisper-large-v3-turbo', 'Whisper Large v3 Turbo (ASR)'),
    ('openai', 'whisper-tiny-en', 'Whisper Tiny EN (ASR)'),
    ('openai', 'whisper', 'Whisper Base (ASR)'),
    ('openai', 'gpt-4o-mini', 'GPT-4o mini'),

    -- Qwen
    ('qwen', 'qwen3.8-27b', 'Qwen 3.8 27B (Vision · Reasoning · Tools)'),
    ('qwen', 'qwen3-30b-a3b-fp8', 'Qwen 3 30B A3B FP8 (Reasoning · Tools)'),
    ('qwen', 'qwen2.5-coder-32b-instruct', 'Qwen 2.5 Coder 32B Instruct'),
    ('qwen', 'qwq-32b', 'QwQ 32B (Reasoning)'),
    ('qwen', 'qwen3-embedding-0.6b', 'Qwen 3 Embedding 0.6B'),

    -- DeepSeek
    ('deepseek', 'deepseek-r1-distill-qwen-32b', 'DeepSeek R1 Distill Qwen 32B'),

    -- Moonshot AI
    ('moonshotai', 'kimi-k2.6', 'Kimi K2.6 (Vision · Reasoning · Tools)'),

    -- NVIDIA
    ('nvidia', 'nemotron-3-120b-a12b', 'Nemotron 3 120B A12B (Reasoning · Tools)'),

    -- Meta
    ('meta', 'llama-4-scout-17b-16e-instruct', 'Llama 4 Scout 17B 16E (Vision · Tools)'),
    ('meta', 'llama-3.3-70b-instruct-fp8-fast', 'Llama 3.3 70B Instruct FP8 Fast'),
    ('meta', 'llama-3.2-11b-vision-instruct', 'Llama 3.2 11B Vision Instruct'),
    ('meta', 'llama-3.1-8b-instruct-fast', 'Llama 3.1 8B Instruct Fast'),
    ('meta', 'llama-3.1-8b-instruct-fp8', 'Llama 3.1 8B Instruct FP8'),
    ('meta', 'llama-guard-3-8b', 'Llama Guard 3 8B (Safety & Moderation)'),
    ('meta', 'llama-3.2-3b-instruct', 'Llama 3.2 3B Instruct'),
    ('meta', 'llama-3.2-1b-instruct', 'Llama 3.2 1B Instruct'),
    ('meta', 'llama-2-7b-chat-hf-lora', 'Llama 2 7B Chat LoRA'),
    ('meta', 'm2m100-1.2b', 'M2M100 1.2B (Multilingual Translation)'),

    -- Google
    ('google', 'gemma-4-26b-a4b-it', 'Gemma 4 26B A4B IT (Vision · Reasoning · Tools)'),
    ('google', 'gemma-sea-lion-v4-27b-it', 'Gemma SEA-LION v4 27B IT'),
    ('google', 'gemma-7b-it-lora', 'Gemma 7B IT LoRA'),
    ('google', 'gemma-2b-it-lora', 'Gemma 2B IT LoRA'),
    ('google', 'embeddinggemma-300m', 'EmbeddingGemma 300M'),

    -- Mistral AI
    ('mistralai', 'mistral-small-3.1-24b-instruct', 'Mistral Small 3.1 24B Instruct (Vision · Tools)'),
    ('mistralai', 'mistral-7b-instruct-v0.2-lora', 'Mistral 7B Instruct v0.2 LoRA'),

    -- Z.AI
    ('zai', 'glm-4.7-flash', 'GLM 4.7 Flash (Reasoning · Tools)'),

    -- IBM
    ('ibm', 'granite-4.0-h-micro', 'Granite 4.0 H Micro (Tools)'),

    -- Black Forest Labs
    ('black-forest-labs', 'flux-2-dev', 'Flux 2 Dev (Image Generation & Edit)'),
    ('black-forest-labs', 'flux-2-klein-9b', 'Flux 2 Klein 9B (Image Generation)'),
    ('black-forest-labs', 'flux-2-klein-4b', 'Flux 2 Klein 4B (Image Generation)'),
    ('black-forest-labs', 'flux-1-schnell', 'Flux 1 Schnell (Fast Image Synthesis)'),

    -- Leonardo
    ('leonardo', 'phoenix-1.0', 'Phoenix 1.0 (Image Generation)'),
    ('leonardo', 'lucid-origin', 'Lucid Origin (Image Generation)'),

    -- Stability & Diffusion
    ('stabilityai', 'stable-diffusion-xl-lightning', 'SDXL Lightning (Image Generation)'),
    ('stabilityai', 'stable-diffusion-xl-base-1.0', 'SDXL Base 1.0 (Image Generation)'),
    ('stabilityai', 'stable-diffusion-v1-5-inpainting', 'SD v1.5 Inpainting'),
    ('stabilityai', 'stable-diffusion-v1-5-img2img', 'SD v1.5 Image-to-Image'),
    ('stabilityai', 'dreamshaper-8-lcm', 'DreamShaper 8 LCM (Image Generation)'),

    -- Deepgram & Speech Audio
    ('deepgram', 'nova-3', 'Deepgram Nova 3 (Realtime ASR)'),
    ('deepgram', 'deepgram-flux', 'Deepgram Flux (Conversational ASR)'),
    ('deepgram', 'aura-2-en', 'Deepgram Aura 2 English (TTS)'),
    ('deepgram', 'aura-2-es', 'Deepgram Aura 2 Spanish (TTS)'),
    ('deepgram', 'aura-1', 'Deepgram Aura 1 (TTS)'),
    ('deepgram', 'melotts', 'MyShell MeloTTS (TTS)'),
    ('deepgram', 'smart-turn-v2', 'Pipecat Smart Turn v2 (Turn Detection)'),

    -- BAAI Embeddings & Rerank
    ('baai', 'bge-m3', 'BGE-M3 (Multilingual Embeddings)'),
    ('baai', 'bge-large-en-v1.5', 'BGE Large EN v1.5 (Embeddings)'),
    ('baai', 'bge-base-en-v1.5', 'BGE Base EN v1.5 (Embeddings)'),
    ('baai', 'bge-small-en-v1.5', 'BGE Small EN v1.5 (Embeddings)'),
    ('baai', 'bge-reranker-base', 'BGE Reranker Base'),
    ('baai', 'plamo-embedding-1b', 'PFNet PLaMo Embedding 1B'),

    -- Multimodal Vision, Translation & Classification
    ('multimodal', 'moondream3.1-9B-A2B', 'Moondream 3.1 9B A2B (Image-to-Text Vision)'),
    ('multimodal', 'llava-1.5-7b-hf', 'LLaVA 1.5 7B (Image-to-Text Vision)'),
    ('multimodal', 'resnet-50', 'Microsoft ResNet-50 (Image Classification)'),
    ('multimodal', 'indictrans2-en-indic-1b', 'AI4Bharat IndicTrans2 1B (Translation)'),
    ('multimodal', 'distilbert-sst-2-int8', 'DistilBERT SST-2 Int8 (Classification)')
ON CONFLICT (provider_id, model_id) DO UPDATE SET
    display_name = EXCLUDED.display_name;
