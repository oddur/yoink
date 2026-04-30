import { createFileRoute } from '@tanstack/react-router';
import { source, blogSource } from '@/lib/source';

const BASE_URL = 'https://yoink.is';

export const Route = createFileRoute('/sitemap.xml')({
  server: {
    handlers: {
      GET() {
        const urls = [
          BASE_URL,
          `${BASE_URL}/about`,
          `${BASE_URL}/blog`,
          ...blogSource.getPages().map((p) => `${BASE_URL}${p.url}`),
          ...source.getPages().map((p) => `${BASE_URL}${p.url}`),
        ];

        const xml = `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${urls.map((url) => `  <url><loc>${url}</loc></url>`).join('\n')}
</urlset>`;

        return new Response(xml, {
          headers: { 'Content-Type': 'application/xml' },
        });
      },
    },
  },
});
