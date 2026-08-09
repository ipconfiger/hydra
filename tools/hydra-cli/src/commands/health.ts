import { Command } from 'commander';
import { HydraClient } from '../client.js';
import { resolveConfig } from '../config.js';
import { printJson, printSuccess } from '../format.js';
import { addGlobalOptions, effectiveOpts, withErrorHandler } from './shared.js';

export function buildHealthCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('health').description('Check Hydra service health.'),
  );
  cmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = (await client.health()) as Record<string, unknown> | null;
      if (opts.json) {
        printJson(res);
        return;
      }
      const status =
        res && typeof res.status === 'string' ? res.status : 'ok';
      printSuccess(`healthy (status: ${status})`);
    }),
  );
  return cmd;
}
