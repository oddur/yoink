import type { APIRoute } from 'astro';
import { getCollection } from 'astro:content';
import { SITE } from '@/lib/site';
import { docHref } from '@/lib/nav';

export const GET: APIRoute = async () => {
  const urls = [
    SITE,
    `${SITE}/about`,
    `${SITE}/blog`,
    `${SITE}/changelog`,
    ...(await getCollection('blog')).map((p) => `${SITE}/blog/${p.id}`),
    ...(await getCollection('docs')).map((p) => SITE + docHref(p.id)),
  ];
  const xml = `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${urls.map((u) => `  <url><loc>${u}</loc></url>`).join('\n')}
</urlset>`;
  return new Response(xml, { headers: { 'Content-Type': 'application/xml' } });
};
