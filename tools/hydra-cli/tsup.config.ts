import { defineConfig } from 'tsup';

// tsup builds the published, shebanged single-file binary: src/cli.ts -> dist/cli.js.
// Tests are compiled separately by `tsc -p tsconfig.test.json` (see `npm test`),
// which preserves `node:` import specifiers that esbuild would otherwise
// normalize to bare builtins.
export default defineConfig({
  name: 'cli',
  entry: ['src/cli.ts'],
  format: ['esm'],
  target: 'node18',
  outDir: 'dist',
  clean: true,
  splitting: false,
  sourcemap: false,
  dts: false,
  platform: 'node',
  banner: {
    js: '#!/usr/bin/env node',
  },
});
