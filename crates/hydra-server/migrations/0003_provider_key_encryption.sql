-- Encrypt provider upstream api-keys at rest (AES-256-GCM).
-- HARD CUTOVER: existing plaintext api_key values are NOT migrated. Operators
-- must provide a master key (HYDRA_ENCRYPTION_KEY / HYDRA_ENCRYPTION_KEY_FILE)
-- and re-enter provider keys via the admin API after upgrading.

ALTER TABLE provider_key ADD COLUMN api_key_ciphertext BLOB;
ALTER TABLE provider_key ADD COLUMN api_key_nonce BLOB;
ALTER TABLE provider_key ADD COLUMN key_version INTEGER NOT NULL DEFAULT 1;

-- Hard cutover: discard legacy plaintext rows so no plaintext lingers.
DELETE FROM provider_key;

-- Drop the old plaintext column so the DB never holds plaintext provider keys.
ALTER TABLE provider_key DROP COLUMN api_key;
