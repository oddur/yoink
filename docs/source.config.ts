import { defineConfig, defineDocs, remarkInclude } from 'fumadocs-mdx/config';
import { remarkCodeTab, remarkMdxMermaid, remarkSteps } from 'fumadocs-core/mdx-plugins';

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

export default defineConfig({
  mdxOptions: {
    remarkPlugins: [remarkInclude, remarkCodeTab, remarkSteps, remarkMdxMermaid],
  },
});
