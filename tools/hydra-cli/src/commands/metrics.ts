import { Command } from 'commander';
import { HydraClient } from '../client.js';
import { resolveConfig } from '../config.js';
import { printJson, printRaw } from '../format.js';
import { addGlobalOptions, effectiveOpts, withErrorHandler } from './shared.js';

export function buildMetricsCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('metrics').description('Dump Prometheus metrics (raw text).'),
  );
  cmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const text = await client.metrics();
      if (opts.json) {
        // No JSON representation exists; emit the text as a JSON string.
        printJson(text);
        return;
      }
      printRaw(text);
    }),
  );
  return cmd;
}
