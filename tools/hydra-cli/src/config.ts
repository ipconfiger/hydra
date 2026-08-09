/**
 * Resolves effective CLI configuration from flags + environment.
 *
 * Global options (available on every command):
 *   --base-url <url>   (env: HYDRA_BASE_URL / HYDRA_HOST)
 *   --token <tok>      (env: HYDRA_ADMIN_TOKEN)   <-- required
 *   --json             raw JSON output, skip table formatting
 *   -v, --verbose      print HTTP method + URL to stderr
 */

const DEFAULT_BASE_URL = 'http://127.0.0.1:8081';

export interface GlobalOpts {
  baseUrl?: string;
  token?: string;
  json?: boolean;
  verbose?: boolean;
}

export interface HydraConfig {
  baseUrl: string;
  token: string;
  json: boolean;
  verbose: boolean;
}

/** Merged options: global options plus any command-specific ones (record index). */
export type EffectiveOpts = GlobalOpts & Record<string, unknown>;

export class ConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ConfigError';
  }
}

/**
 * Resolve final configuration. Precedence: explicit flag > env var > default.
 * Throws ConfigError (caught by the action wrapper) if the token is missing.
 */
export function resolveConfig(opts: GlobalOpts): HydraConfig {
  const rawBase =
    opts.baseUrl ??
    process.env.HYDRA_BASE_URL ??
    process.env.HYDRA_HOST ??
    DEFAULT_BASE_URL;
  const baseUrl = rawBase.replace(/\/+$/, '');

  const token = (opts.token ?? process.env.HYDRA_ADMIN_TOKEN ?? '').trim();
  if (!token) {
    throw new ConfigError(
      'Admin token is required. Pass --token <tok> or set the HYDRA_ADMIN_TOKEN environment variable.',
    );
  }

  return {
    baseUrl,
    token,
    json: opts.json ?? false,
    verbose: opts.verbose ?? false,
  };
}
