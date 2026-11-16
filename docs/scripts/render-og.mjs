// Renders public/og.png (1200 x 630) from scripts/og.html with headless Chrome.
//   pnpm og
// Chrome does not load fonts from file:// URLs, so the pack face is inlined
// into a temporary copy of the page first.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const chrome = process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const font = readFileSync(
  join(root, 'node_modules/@fontsource-variable/archivo/files/archivo-latin-wdth-normal.woff2'),
).toString('base64');
const html = readFileSync(join(root, 'scripts/og.html'), 'utf8').replace(
  /url\([^)]*archivo-latin-wdth-normal\.woff2\)/,
  `url(data:font/woff2;base64,${font})`,
);
const page = join(mkdtempSync(join(tmpdir(), 'yoink-og-')), 'og.html');
writeFileSync(page, html);
execFileSync(chrome, [
  '--headless=new', '--hide-scrollbars', '--window-size=1200,630', '--virtual-time-budget=2000',
  `--screenshot=${join(root, 'public/og.png')}`, `file://${page}`,
], { stdio: 'ignore' });
console.log('wrote public/og.png');
