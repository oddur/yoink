import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { z } from 'astro/zod';

// `intro/index.mdx` -> `intro`, root `index.mdx` -> `index`. Keeps the
// URLs the old site served (`/docs/explanation`, `/docs/explanation/compared`).
const idFromPath = ({ entry }: { entry: string }) => {
  const id = entry.replace(/\.mdx?$/, '').replace(/(^|\/)index$/, '');
  return id === '' ? 'index' : id;
};

const page = z.looseObject({
  title: z.string(),
  description: z.string().optional(),
});

export const collections = {
  docs: defineCollection({
    loader: glob({ pattern: '**/*.{md,mdx}', base: './content/docs', generateId: idFromPath }),
    schema: page,
  }),
  blog: defineCollection({
    loader: glob({ pattern: '*.{md,mdx}', base: './content/blog' }),
    schema: page.extend({ date: z.coerce.date(), author: z.string().optional() }),
  }),
  changelog: defineCollection({
    loader: glob({ pattern: '*.md', base: './content/changelog' }),
    schema: z.object({ version: z.string(), date: z.coerce.date() }),
  }),
  about: defineCollection({
    loader: glob({ pattern: 'about.mdx', base: './content' }),
    schema: page,
  }),
};
