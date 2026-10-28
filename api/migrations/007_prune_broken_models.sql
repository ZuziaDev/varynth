-- Remove all models from `models`, `provider_models`, and `provider_catalog`
-- that are unsupported, broken, or lack chat completion support on the upstream cluster.
-- Only the 21 verified working Chat & Reasoning models remain active.

DELETE FROM models
WHERE id NOT LIKE 'varynth/public-%'
  AND id NOT IN (
    'varynth/gpt-oss-120b',
    'varynth/qwen3.8-27b',
    'varynth/nemotron-3-120b-a12b',
    'varynth/qwen3-30b-a3b-fp8',
    'varynth/qwen2.5-coder-32b-instruct',
    'varynth/deepseek-r1-distill-qwen-32b',
    'varynth/qwq-32b',
    'varynth/llama-4-scout-17b-16e-instruct',
    'varynth/llama-3.3-70b-instruct-fp8-fast',
    'varynth/gemma-4-26b-a4b-it',
    'varynth/glm-4.7-flash',
    'varynth/mistral-small-3.1-24b-instruct',
    'varynth/gpt-oss-20b',
    'varynth/gemma-sea-lion-v4-27b-it',
    'varynth/llama-3.1-8b-instruct-fast',
    'varynth/llama-3.1-8b-instruct-fp8',
    'varynth/granite-4.0-h-micro',
    'varynth/llama-3.2-3b-instruct',
    'varynth/llama-3.2-1b-instruct',
    'varynth/gemma-7b-it-lora',
    'varynth/gemma-2b-it-lora'
  );

DELETE FROM provider_models
WHERE model_id NOT IN (
    'gpt-oss-120b',
    'qwen3.8-27b',
    'nemotron-3-120b-a12b',
    'qwen3-30b-a3b-fp8',
    'qwen2.5-coder-32b-instruct',
    'deepseek-r1-distill-qwen-32b',
    'qwq-32b',
    'llama-4-scout-17b-16e-instruct',
    'llama-3.3-70b-instruct-fp8-fast',
    'gemma-4-26b-a4b-it',
    'glm-4.7-flash',
    'mistral-small-3.1-24b-instruct',
    'gpt-oss-20b',
    'gemma-sea-lion-v4-27b-it',
    'llama-3.1-8b-instruct-fast',
    'llama-3.1-8b-instruct-fp8',
    'granite-4.0-h-micro',
    'llama-3.2-3b-instruct',
    'llama-3.2-1b-instruct',
    'gemma-7b-it-lora',
    'gemma-2b-it-lora',
    'gpt-4o-mini'
  );

DELETE FROM provider_catalog
WHERE id NOT IN (SELECT DISTINCT provider_id FROM provider_models);
