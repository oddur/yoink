import { createRootRoute, HeadContent, Outlet, Scripts, useRouterState } from '@tanstack/react-router';
import * as React from 'react';
import appCss from '@/styles/app.css?url';
import { RootProvider } from 'fumadocs-ui/provider/tanstack';
import SearchDialog from '@/components/search';
import { init, trackEvent } from '@aptabase/web';

const APTABASE_KEY = 'A-EU-6868521188';

export const Route = createRootRoute({
  head: () => ({
    meta: [
      {
        charSet: 'utf-8',
      },
      {
        name: 'viewport',
        content: 'width=device-width, initial-scale=1',
      },
      { title: 'yoink — docs' },
      { name: 'description', content: 'A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of VPS or bare-metal hosts.' },
      { property: 'og:type', content: 'website' },
      { property: 'og:site_name', content: 'yoink' },
      { property: 'og:title', content: 'yoink — docs' },
      { property: 'og:description', content: 'A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of VPS or bare-metal hosts.' },
      { property: 'og:image', content: 'https://yoink.is/og.png' },
      { property: 'og:url', content: 'https://yoink.is' },
      { name: 'twitter:card', content: 'summary_large_image' },
      { name: 'twitter:image', content: 'https://yoink.is/og.png' },
    ],
    links: [
      { rel: 'stylesheet', href: appCss },
      { rel: 'icon', href: '/favicon.svg', type: 'image/svg+xml' },
    ],
  }),
  component: RootComponent,
});

function Analytics() {
  const location = useRouterState({ select: (s) => s.location });

  React.useEffect(() => {
    init(APTABASE_KEY);
  }, []);

  React.useEffect(() => {
    trackEvent('page_view', { path: location.pathname });
  }, [location.pathname]);

  return null;
}

function RootComponent() {
  return (
    <html lang="en" suppressHydrationWarning>
      <head>
        <HeadContent />
      </head>
      <body className="flex flex-col min-h-screen">
        <RootProvider
          search={{ SearchDialog }}
          theme={{ attribute: 'class', defaultTheme: 'system', disableTransitionOnChange: true }}
        >
          <Analytics />
          <Outlet />
        </RootProvider>
        <Scripts />
      </body>
    </html>
  );
}
