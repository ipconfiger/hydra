import { Command } from 'commander';
import type { EffectiveOpts, GlobalOpts } from '../config.js';

/** Attach the four global options to a command (used on the root + every leaf). */
export function addGlobalOptions(cmd: Command): Command {
  cmd
    .option('--base-url <url>', 'Hydra base URL (env: HYDRA_BASE_URL, HYDRA_HOST)')
    .option('--token <tok>', 'Admin bearer token (env: HYDRA_ADMIN_TOKEN)')
    .option('--json', 'Raw JSON output, skip table formatting')
    .option('-v, --verbose', 'Print HTTP method + URL to stderr');
  return cmd;
}

function rootOf(cmd: Command): Command {
  let c: Command = cmd;
  while (c.parent) c = c.parent;
  return c;
}

/**
 * Merge root-level options with the leaf command's options. Leaf values win,
 * but undefined leaf values never clobber a value supplied at the root (so
 * `hydra-admin --token X providers list` and `hydra-admin providers list --token X`
 * both work).
 */
export function effectiveOpts(cmd: Command): EffectiveOpts {
  const root = rootOf(cmd).opts() as GlobalOpts;
  const leaf = cmd.opts() as Record<string, unknown>;
  const merged: Record<string, unknown> = { ...root };
  for (const [k, v] of Object.entries(leaf)) {
    if (v !== undefined) merged[k] = v;
  }
  return merged as EffectiveOpts;
}

type AsyncAction = (...args: unknown[]) => Promise<void> | void;

/**
 * Wrap an async commander action so any thrown Error is reported as
 * `Error: <message>` on stderr and the process exits with status 1.
 */
export function withErrorHandler(fn: AsyncAction): AsyncAction {
  return async (...args: unknown[]) => {
    try {
      await fn(...args);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.error(`Error: ${msg}`);
      process.exit(1);
    }
  };
}
