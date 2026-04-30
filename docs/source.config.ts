import { defineConfig, defineDocs } from 'fumadocs-mdx/config';

export const docs = defineDocs({
  dir: 'content/docs',
  docs: {
    postprocess: {
      includeProcessedMarkdown: true,
    },
  },
});

export const blog = defineDocs({
  dir: 'content/blog',
});

export const about = defineDocs({
  dir: 'content',
  docs: { files: ['about.mdx'] },
  meta: { files: [] },
});

export default defineConfig();
