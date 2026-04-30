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

const listPosts = createServerFn({ method: 'GET' })
  .middleware([staticFunctionMiddleware])
  .handler(async () => {
    const pages = blogSource.getPages();
    const posts = pages
      .map((p) => ({
        slug: p.slugs.join('/'),
        title: p.data.title as string,
        description: (p.data.description ?? '') as string,
        date: (p.data.date ?? '') as string,
      }))
      .sort((a, b) => b.date.localeCompare(a.date));
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
                {post.date && (
                  <time className="text-sm text-fd-muted-foreground mt-1 block">
                    {new Date(post.date).toLocaleDateString('en-US', {
                      year: 'numeric',
                      month: 'long',
                      day: 'numeric',
                    })}
                  </time>
                )}
                {post.description && (
                  <p className="text-fd-muted-foreground mt-2">{post.description}</p>
                )}
              </article>
            ))}
          </div>
        </DocsBody>
      </DocsPage>
    </DocsLayout>
  );
}
