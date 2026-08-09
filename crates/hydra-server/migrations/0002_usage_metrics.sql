-- Wave-6: granular usage metrics — token breakdown + latency dimensions.
--
-- Adds three nullable columns to `usage_record` (design §9.1):
--   cached_tokens      — prompt-cache hit token count (OpenAI
--                        prompt_tokens_details.cached_tokens / Anthropic
--                        cache_read_input_tokens). NULL when the provider
--                        does not report it.
--   ttft_ms            — Time To First Token: request start → first response
--                        chunk. NULL for non-streamed / errored requests.
--   forward_latency_ms — Hydra's own overhead: request start → just before
--                        the upstream send (auth + routing + body read).
--
-- All three are nullable so pre-existing rows (and the migrate-idempotent
-- re-run path) keep working; new inserts populate them when known.
ALTER TABLE usage_record ADD COLUMN cached_tokens INTEGER;
ALTER TABLE usage_record ADD COLUMN ttft_ms INTEGER;
ALTER TABLE usage_record ADD COLUMN forward_latency_ms INTEGER;
