import type { CollectionEntry } from 'astro:content';
import { SITE } from './site';
import { docHref } from './nav';

export const mdPath = (id: string) => `/llms.mdx/docs/${id === 'index' ? '' : id + '/'}content.md`;

export const pageMarkdown = (e: CollectionEntry<'docs'>) =>
  `# ${e.data.title} (${SITE}${docHref(e.id)})\n\n${(e.body ?? '').trim()}\n`;
