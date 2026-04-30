import { createFileRoute, notFound } from '@tanstack/react-router';
import { createServerFn } from '@tanstack/react-start';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { staticFunctionMiddleware } from '@tanstack/start-static-server-functions';
import { useFumadocsLoader } from 'fumadocs-core/source/client';
import { aboutSource } from '@/lib/source';
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
    return { path: page.path };
  });

const clientLoader = browserCollections.about.createClientLoader({
  component({ default: MDX, frontmatter }) {
    return (
      <div className="container max-w-2xl mx-auto px-4 py-12">
        <div className="prose max-w-none">
          <MDX components={useMDXComponents()} />
        </div>
      </div>
    );
  },
});

function AboutPage() {
  const { path } = useFumadocsLoader(Route.useLoaderData());
  return (
    <HomeLayout {...baseOptions()}>
      <Suspense>{clientLoader.useContent(path, {})}</Suspense>
    </HomeLayout>
  );
}
