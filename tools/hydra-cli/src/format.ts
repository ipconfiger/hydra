/** Output helpers: raw JSON, hand-rolled compact tables, one-line confirmations. */

export interface ColDef {
  field: string;
  header: string;
  width: number;
}

const NO_COLOR = process.env.NO_COLOR !== undefined && process.env.NO_COLOR !== '';

// Subtle ANSI dimming for table chrome; disabled when NO_COLOR is set.
const dim = (s: string): string => (NO_COLOR ? s : `\x1b[2m${s}\x1b[0m`);

export function printJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`);
}

/** Print arbitrary text ensuring exactly one trailing newline. */
export function printRaw(text: string): void {
  process.stdout.write(text.endsWith('\n') ? text : `${text}\n`);
}

function padCell(value: string, width: number): string {
  const single = value.replace(/[\r\n]+/g, ' ');
  if (single.length > width) {
    return `${single.slice(0, Math.max(0, width - 1))}…`;
  }
  return single + ' '.repeat(width - single.length);
}

export function printTable(
  columns: ColDef[],
  rows: Array<Record<string, unknown>>,
): void {
  if (rows.length === 0) {
    console.log('(no records)');
    return;
  }
  console.log(dim(columns.map((c) => padCell(c.header, c.width)).join('  ')));
  console.log(dim(columns.map((c) => '-'.repeat(c.width)).join('  ')));
  for (const row of rows) {
    console.log(
      columns.map((c) => padCell(String(row[c.field] ?? ''), c.width)).join('  '),
    );
  }
  console.log(dim(`\n${rows.length} record${rows.length === 1 ? '' : 's'}`));
}

export function printSuccess(message: string): void {
  const mark = NO_COLOR ? '\u2713' : '\x1b[32m\u2713\x1b[0m';
  console.log(`${mark} ${message}`);
}

export function printNotice(message: string): void {
  const mark = NO_COLOR ? 'i' : '\x1b[36mi\x1b[0m';
  console.log(`${mark} ${message}`);
}
