// Generates public/og.png at build time using satori + resvg.
// Run via `pnpm prebuild` (wired into package.json scripts).
import satori from 'satori';
import { Resvg } from '@resvg/resvg-js';
import { readFileSync, writeFileSync, mkdirSync } from 'fs';
import { fileURLToPath } from 'url';
import { join, dirname } from 'path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = join(__dirname, '..');

const fontData = readFileSync(
  join(root, 'node_modules/@fontsource/inter/files/inter-latin-400-normal.woff'),
);

const svg = await satori(
  {
    type: 'div',
    props: {
      style: {
        display: 'flex',
        flexDirection: 'column',
        justifyContent: 'center',
        alignItems: 'flex-start',
        width: '100%',
        height: '100%',
        padding: '72px 80px',
        background: '#09090b',
        gap: 24,
      },
      children: [
        {
          type: 'div',
          props: {
            style: { fontSize: 72, fontWeight: 700, color: '#fff', letterSpacing: '-2px' },
            children: '🪝 yoink',
          },
        },
        {
          type: 'div',
          props: {
            style: { fontSize: 28, color: '#a1a1aa', maxWidth: 720, lineHeight: 1.4 },
            children: 'A small, opinionated container deploy CLI + TUI for bare-metal hosts.',
          },
        },
        {
          type: 'div',
          props: {
            style: { fontSize: 20, color: '#52525b', marginTop: 8 },
            children: 'yoink.is',
          },
        },
      ],
    },
  },
  {
    width: 1200,
    height: 630,
    fonts: [{ name: 'Inter', data: fontData, weight: 400, style: 'normal' }],
  },
);

const resvg = new Resvg(svg, { fitTo: { mode: 'width', value: 1200 } });
const png = resvg.render().asPng();

mkdirSync(join(root, 'public'), { recursive: true });
writeFileSync(join(root, 'public/og.png'), png);
console.log('✓ public/og.png generated');
