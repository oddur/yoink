import { createFileRoute, notFound } from '@tanstack/react-router';
import { createServerFn } from '@tanstack/react-start';
import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { DocsBody, DocsPage } from 'fumadocs-ui/layouts/docs/page';
import { staticFunctionMiddleware } from '@tanstack/start-static-server-functions';
import { useFumadocsLoader } from 'fumadocs-core/source/client';
import { aboutSource, source } from '@/lib/source';
import { baseOptions } from '@/lib/layout.shared';
import { useMDXComponents } from '@/components/mdx';
import { Suspense } from 'react';
import browserCollections from 'collections/browser';

export const Route = createFileRoute('/about')({
  component: AboutPage,
  loader: async () => {
    const data = await loadAbout();
    await clientLoader.preload(data.path);
    return data;
  },
});

const loadAbout = createServerFn({ method: 'GET' })
  .middleware([staticFunctionMiddleware])
  .handler(async () => {
    const page = aboutSource.getPage(['about']);
    if (!page) throw notFound();
    return {
      path: page.path,
      pageTree: await source.serializePageTree(source.getPageTree()),
    };
  });

const clientLoader = browserCollections.about.createClientLoader({
  component({ default: MDX }) {
    return (
      <DocsPage toc={[]}>
        <DocsBody className="prose">
          <MDX components={useMDXComponents()} />
        </DocsBody>
      </DocsPage>
    );
  },
});

function AboutPage() {
  const { path, pageTree } = useFumadocsLoader(Route.useLoaderData());
  return (
    <DocsLayout {...baseOptions()} tree={pageTree}>
      <Suspense>{clientLoader.useContent(path, {})}</Suspense>
    </DocsLayout>
  );
}
