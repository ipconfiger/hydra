import { Command } from 'commander';
import { HydraClient } from '../client.js';
import { resolveConfig } from '../config.js';
import { printJson, printSuccess } from '../format.js';
import { addGlobalOptions, effectiveOpts, withErrorHandler } from './shared.js';

export function buildReloadCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('reload').description('Hot-reload the Hydra config snapshot.'),
  );
  cmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = await client.reload();
      if (opts.json) {
        printJson(res);
        return;
      }
      printSuccess('config reloaded');
    }),
  );
  return cmd;
}
