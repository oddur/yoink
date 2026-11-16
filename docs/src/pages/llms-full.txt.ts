import type { APIRoute } from 'astro';
import { getCollection } from 'astro:content';
import { getNav } from '@/lib/nav';
import { pageMarkdown } from '@/lib/llms';

export const GET: APIRoute = async () => {
  const { order } = await getNav();
  const byId = new Map((await getCollection('docs')).map((e) => [e.id, e]));
  const body = order.map((l) => pageMarkdown(byId.get(l.id)!)).join('\n\n');
  return new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
