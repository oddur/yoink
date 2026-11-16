import type { APIRoute } from 'astro';
import { getCollection } from 'astro:content';
import { pageMarkdown } from '@/lib/llms';

// /llms.mdx/docs/<page>/content.md: the Markdown source of each docs page.
export async function getStaticPaths() {
  return (await getCollection('docs')).map((entry) => ({
    params: { slug: entry.id === 'index' ? 'content.md' : `${entry.id}/content.md` },
    props: { entry },
  }));
}

export const GET: APIRoute = ({ props }) =>
  new Response(pageMarkdown(props.entry), { headers: { 'Content-Type': 'text/markdown; charset=utf-8' } });
