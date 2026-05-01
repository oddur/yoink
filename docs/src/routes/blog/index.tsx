import { createFileRoute, Link } from '@tanstack/react-router';
import { createServerFn } from '@tanstack/react-start';
import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { DocsBody, DocsPage } from 'fumadocs-ui/layouts/docs/page';
import { staticFunctionMiddleware } from '@tanstack/start-static-server-functions';
import { deserializePageTree } from 'fumadocs-core/source/client';
import { blogSource, source } from '@/lib/source';
import { baseOptions } from '@/lib/layout.shared';

export const Route = createFileRoute('/blog/')({
  component: BlogIndex,
  loader: () => listPosts(),
});

const firstParagraph = (markdown: string): string => {
  const blocks = markdown.split(/\n\s*\n/);
  for (const raw of blocks) {
    const block = raw.trim();
    if (!block) continue;
    if (block.startsWith('#')) continue;
    if (block.startsWith('```')) continue;
    if (block.startsWith('import ') || block.startsWith('export ')) continue;
    return block.replace(/\s+/g, ' ');
  }
  return '';
};

const listPosts = createServerFn({ method: 'GET' })
  .middleware([staticFunctionMiddleware])
  .handler(async () => {
    const pages = blogSource.getPages();
    const posts = await Promise.all(
      pages.map(async (p) => {
        const d = p.data as Record<string, unknown> & {
          getText?: (type: 'raw' | 'processed') => Promise<string>;
          _markdown?: string;
        };
        const rawDate = d.date;
        const dateStr =
          rawDate instanceof Date
            ? rawDate.toISOString().slice(0, 10)
            : typeof rawDate === 'string'
              ? rawDate
              : '';
        let excerpt = (d.description ?? '') as string;
        try {
          if (typeof d.getText === 'function') {
            const md = await d.getText('processed');
            const para = firstParagraph(md);
            if (para) excerpt = para;
          } else if (typeof d._markdown === 'string') {
            const para = firstParagraph(d._markdown);
            if (para) excerpt = para;
          }
        } catch {
          // fall back to description
        }
        return {
          slug: p.slugs.join('/'),
          title: d.title as string,
          excerpt,
          date: dateStr,
          author: (d.author ?? '') as string,
        };
      }),
    );
    posts.sort((a, b) => b.date.localeCompare(a.date));
    return {
      posts,
      pageTree: await source.serializePageTree(source.getPageTree()),
    };
  });

function BlogIndex() {
  const { posts, pageTree } = Route.useLoaderData();

  return (
    <DocsLayout {...baseOptions()} tree={deserializePageTree(pageTree)}>
      <DocsPage toc={[]}>
        <DocsBody>
          <h1 className="text-3xl font-bold mb-8">Blog</h1>
          <div className="flex flex-col gap-6">
            {posts.map((post) => (
              <article key={post.slug}>
                <Link to="/blog/$slug" params={{ slug: post.slug }}>
                  <h2 className="text-xl font-semibold hover:text-fd-primary transition-colors">
                    {post.title}
                  </h2>
                </Link>
                {(post.date || post.author) && (
                  <p className="text-sm text-fd-muted-foreground mt-1">
                    {post.date && (
                      <time>
                        {new Date(post.date).toLocaleDateString('en-US', {
                          year: 'numeric',
                          month: 'long',
                          day: 'numeric',
                        })}
                      </time>
                    )}
                    {post.date && post.author && ' · '}
                    {post.author && <span>{post.author}</span>}
                  </p>
                )}
                {post.excerpt && (
                  <p className="text-fd-muted-foreground mt-2">{post.excerpt}</p>
                )}
              </article>
            ))}
          </div>
        </DocsBody>
      </DocsPage>
    </DocsLayout>
  );
}
