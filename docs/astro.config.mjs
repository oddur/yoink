import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import { unified } from '@astrojs/markdown-remark';
import { remarkMermaid, remarkSteps } from './src/lib/remark-mermaid.mjs';
import { rehypeTableWrap } from './src/lib/rehype-table-wrap.mjs';

export default defineConfig({
  site: 'https://yoink.is',
  trailingSlash: 'never',
  build: { format: 'directory' },
  server: { port: 3000 },
  integrations: [mdx()],
  markdown: {
    // The remark/rehype pipeline: Mermaid fences render to SVG at build
    // time, [step] headings get numbered, and tables get a scroll container.
    processor: unified({
      remarkPlugins: [remarkMermaid, remarkSteps],
      rehypePlugins: [rehypeTableWrap],
    }),
    shikiConfig: { theme: 'vitesse-dark', wrap: false },
  },
});
