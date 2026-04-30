import { createFileRoute, Link } from '@tanstack/react-router';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { baseOptions } from '@/lib/layout.shared';

export const Route = createFileRoute('/')({
  component: Home,
});

function Home() {
  return (
    <HomeLayout {...baseOptions()}>
      <div className="flex flex-col items-center justify-center text-center flex-1 py-24 px-4">
        <h1 className="text-4xl font-bold mb-4">🪝 yoink</h1>
        <p className="text-fd-muted-foreground text-lg mb-8 max-w-md">
          A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts.
        </p>
        <div className="flex gap-3">
          <Link
            to="/docs/$"
            params={{ _splat: 'intro/what-and-why' }}
            className="px-4 py-2 rounded-lg bg-fd-primary text-fd-primary-foreground font-medium text-sm"
          >
            Get started
          </Link>
          <a
            href="https://github.com/oddur/yoink"
            className="px-4 py-2 rounded-lg border border-fd-border text-fd-foreground font-medium text-sm hover:bg-fd-accent"
          >
            GitHub
          </a>
        </div>
      </div>
    </HomeLayout>
  );
}
