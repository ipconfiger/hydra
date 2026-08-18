-- Neutral token accounting columns for `usage_record` (design §9.1/§9.5).
--
-- The metering table must be provider-neutral: it records what happened in
-- token terms, not which upstream schema produced the numbers. Column names
-- no longer mirror OpenAI's (`prompt_tokens` / `completion_tokens`) or
-- Anthropic's (`input_tokens` / `output_tokens`) vocabulary:
--
--   prompt_tokens      -> tokens_in          (tokens SENT in the request,
--                                             cache hits included)
--   completion_tokens  -> tokens_out         (tokens RETURNED by the model)
--   cached_tokens      -> cache_hit_tokens   (tokens that hit the prompt
--                                             cache; a subset of tokens_in)
--   total_tokens       -> DROPPED            (derived tokens_in+tokens_out;
--                                             carries no billing meaning)
--
-- SQLite RENAME COLUMN / DROP COLUMN require 3.25+ / 3.35+ respectively;
-- the bundled libsqlite3-sys satisfies both.
ALTER TABLE usage_record RENAME COLUMN prompt_tokens TO tokens_in;
ALTER TABLE usage_record RENAME COLUMN completion_tokens TO tokens_out;
ALTER TABLE usage_record RENAME COLUMN cached_tokens TO cache_hit_tokens;
ALTER TABLE usage_record DROP COLUMN total_tokens;
