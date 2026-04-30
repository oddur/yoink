import { loader } from 'fumadocs-core/source';
import { lucideIconsPlugin } from 'fumadocs-core/source/lucide-icons';
import { docs, blog as blogCollection, about as aboutCollection } from 'collections/server';
import { docsContentRoute, docsRoute } from './shared';

export const source = loader({
  source: docs.toFumadocsSource(),
  baseUrl: docsRoute,
  plugins: [lucideIconsPlugin()],
});

export const blogSource = loader({
  source: blogCollection.toFumadocsSource(),
  baseUrl: '/blog',
});

export const aboutSource = loader({
  source: aboutCollection.toFumadocsSource(),
  baseUrl: '/about',
});

export function getPageMarkdownUrl(page: (typeof source)['$inferPage']) {
  const segments = [...page.slugs, 'content.md'];

  return {
    segments,
    url: `${docsContentRoute}/${segments.join('/')}`,
  };
}

export async function getLLMText(page: (typeof source)['$inferPage']) {
  const processed = await page.data.getText('processed');

  return `# ${page.data.title} (${page.url})

${processed}`;
}
