import { createFileRoute, notFound } from '@tanstack/react-router';
import { createServerFn } from '@tanstack/react-start';
import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { DocsBody, DocsPage } from 'fumadocs-ui/layouts/docs/page';
import { staticFunctionMiddleware } from '@tanstack/start-static-server-functions';
import { useFumadocsLoader } from 'fumadocs-core/source/client';
import { blogSource, source } from '@/lib/source';
import { baseOptions } from '@/lib/layout.shared';
import { useMDXComponents } from '@/components/mdx';
import { Suspense } from 'react';
import browserCollections from 'collections/browser';

export const Route = createFileRoute('/blog/$slug')({
  component: BlogPost,
  loader: async ({ params }) => {
    const data = await loadPost({ data: params.slug });
    await clientLoader.preload(data.path);
    return data;
  },
});

const loadPost = createServerFn({ method: 'GET' })
  .inputValidator((slug: string) => slug)
  .middleware([staticFunctionMiddleware])
  .handler(async ({ data: slug }) => {
    const slugs = slug.split('/');
    const page = blogSource.getPage(slugs);
    if (!page) throw notFound();
    return {
      path: page.path,
      pageTree: await source.serializePageTree(source.getPageTree()),
    };
  });

const clientLoader = browserCollections.blog.createClientLoader({
  component({ default: MDX, frontmatter }) {
    return (
      <DocsPage toc={[]}>
        <DocsBody>
          <h1 className="text-3xl font-bold mb-2">{frontmatter.title as string}</h1>
          {frontmatter.date && (
            <time className="text-sm text-fd-muted-foreground block mb-8">
              {new Date(frontmatter.date as string).toLocaleDateString('en-US', {
                year: 'numeric',
                month: 'long',
                day: 'numeric',
              })}
            </time>
          )}
          <div className="prose max-w-none">
            <MDX components={useMDXComponents()} />
          </div>
        </DocsBody>
      </DocsPage>
    );
  },
});

function BlogPost() {
  const { path, pageTree } = useFumadocsLoader(Route.useLoaderData());
  return (
    <DocsLayout {...baseOptions()} tree={pageTree}>
      <Suspense>{clientLoader.useContent(path, {})}</Suspense>
    </DocsLayout>
  );
}
