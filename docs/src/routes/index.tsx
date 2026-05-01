import { createFileRoute, Link } from '@tanstack/react-router';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { Card, Cards } from 'fumadocs-ui/components/card';
import { baseOptions } from '@/lib/layout.shared';
import { blogSource } from '@/lib/source';
import { TerminalPlayer } from '@/components/terminal-player';
import {
  Globe,
  HeartPulse,
  Layers,
  Server,
  ShieldCheck,
  Terminal,
} from 'lucide-react';
import { Suspense } from 'react';

export const Route = createFileRoute('/')({
  component: Home,
  loader: () => {
    const pages = blogSource.getPages();
    if (!pages.length) return null;
    const withDates = pages.map((p) => {
      const d = p.data as Record<string, unknown>;
      const rawDate = d.date;
      const dateStr =
        rawDate instanceof Date
          ? rawDate.toISOString().slice(0, 10)
          : typeof rawDate === 'string'
            ? rawDate
            : '';
      return { slug: p.slugs.join('/'), title: d.title as string, date: dateStr };
    });
    withDates.sort((a, b) => b.date.localeCompare(a.date));
    return withDates[0] ?? null;
  },
});

const features = [
  {
    icon: <Server className="size-5" />,
    title: 'SSH is the only dependency',
    description: 'Key-based SSH access to any host is all yoink needs. No agent on the server, no registry account required.',
  },
  {
    icon: <ShieldCheck className="size-5" />,
    title: 'Sealed secrets in the repo',
    description: 'AGE-encrypt any value and commit secrets.age. Zero plaintext on disk or in CI logs.',
  },
  {
    icon: <HeartPulse className="size-5" />,
    title: 'Healthcheck-gated swaps',
    description: 'New container gets traffic only after /health returns 200. Old one stays live if it fails.',
  },
  {
    icon: <Globe className="size-5" />,
    title: 'HTTPS in one field',
    description: 'Bundled Caddy. Add domain: myapp.example.com and automatic certs follow.',
  },
  {
    icon: <Layers className="size-5" />,
    title: 'Dependency-ordered waves',
    description: 'depends_on topological sort ensures database before API before web, every deploy.',
  },
  {
    icon: <Terminal className="size-5" />,
    title: 'Full TUI',
    description: 'Live logs, drift cells, deploy status — a full terminal interface alongside the CLI.',
  },
];

function Home() {
  const latestPost = Route.useLoaderData();
  return (
    <HomeLayout {...baseOptions()}>
      {/* ── Hero ─────────────────────────────────────────────────────── */}
      <section className="flex flex-col items-center justify-center text-center py-20 px-4">
        <h1 className="text-5xl sm:text-6xl font-bold tracking-tight mb-4">
          🪝 yoink
        </h1>

        <p className="text-2xl sm:text-3xl font-semibold text-fd-foreground mb-3">
          Container deploys for the servers you run.
        </p>

        <p className="text-fd-muted-foreground text-lg mb-8 max-w-lg">
          A small, opinionated deploy CLI + TUI for people who run a handful of services on a handful of VPS or bare-metal hosts.
        </p>

        <div className="flex flex-wrap gap-3 justify-center mb-10">
          <Link
            to="/docs/$"
            params={{ _splat: 'start/first-deploy' }}
            className="px-5 py-2.5 rounded-lg bg-fd-primary text-fd-primary-foreground font-medium text-sm hover:opacity-90 transition-opacity"
          >
            First deploy →
          </Link>
          <Link
            to="/docs/$"
            params={{ _splat: 'intro/what-and-why' }}
            className="px-5 py-2.5 rounded-lg border border-fd-border text-fd-foreground font-medium text-sm hover:bg-fd-accent transition-colors"
          >
            What &amp; why
          </Link>
          <a
            href="https://github.com/oddur/yoink"
            className="px-5 py-2.5 rounded-lg border border-fd-border text-fd-foreground font-medium text-sm hover:bg-fd-accent transition-colors"
          >
            GitHub
          </a>
        </div>

        {/* Install pill */}
        <div className="flex items-center gap-3 px-4 py-2 rounded-lg bg-fd-muted/50 border border-fd-border font-mono text-sm">
          <span className="text-fd-muted-foreground select-none">$</span>
          <code className="text-fd-foreground">brew install oddur/yoink/yoink</code>
        </div>
      </section>

      {/* ── Terminal demo ─────────────────────────────────────────────── */}
      <section className="px-4 pb-16 max-w-4xl mx-auto w-full">
        <div className="rounded-xl overflow-hidden border border-fd-border bg-[#1a1a2e] shadow-2xl">
          {/* fake title bar */}
          <div className="flex items-center gap-1.5 px-4 py-3 border-b border-white/10 bg-[#16162a]">
            <span className="size-3 rounded-full bg-red-500/80" />
            <span className="size-3 rounded-full bg-yellow-500/80" />
            <span className="size-3 rounded-full bg-green-500/80" />
            <span className="ml-3 text-xs text-white/30 font-mono">yoink up --build</span>
          </div>
          <Suspense fallback={<div className="h-72 bg-[#1a1a2e]" />}>
            <TerminalPlayer src="/demo.cast" />
          </Suspense>
        </div>
        <p className="text-center text-xs text-fd-muted-foreground mt-3">
          Rolling deploy — 4 replicas, healthcheck-gated, no downtime.
        </p>
      </section>

      {/* ── Feature grid ──────────────────────────────────────────────── */}
      <section className="px-4 pb-16 max-w-5xl mx-auto w-full">
        <div className="grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 gap-4">
          {features.map((f) => (
            <div
              key={f.title}
              className="flex flex-col gap-2 p-5 rounded-xl border border-fd-border bg-fd-card hover:border-fd-ring/50 transition-colors"
            >
              <div className="text-fd-primary">{f.icon}</div>
              <p className="font-semibold text-fd-foreground">{f.title}</p>
              <p className="text-sm text-fd-muted-foreground leading-relaxed">{f.description}</p>
            </div>
          ))}
        </div>
      </section>

      {/* ── Latest blog post ──────────────────────────────────────────── */}
      {latestPost && (
        <section className="px-4 pb-10 max-w-5xl mx-auto w-full">
          <Link
            to="/blog/$slug"
            params={{ slug: latestPost.slug }}
            className="flex items-center justify-between gap-4 px-5 py-4 rounded-xl border border-fd-border bg-fd-card hover:border-fd-ring/50 transition-colors group"
          >
            <div className="flex items-center gap-3 min-w-0">
              <span className="shrink-0 text-xs font-medium text-fd-primary bg-fd-primary/10 px-2 py-0.5 rounded-full">
                New post
              </span>
              <span className="text-sm font-medium text-fd-foreground truncate group-hover:text-fd-primary transition-colors">
                {latestPost.title}
              </span>
            </div>
            {latestPost.date && (
              <time className="shrink-0 text-xs text-fd-muted-foreground">
                {new Date(latestPost.date).toLocaleDateString('en-US', {
                  year: 'numeric',
                  month: 'short',
                  day: 'numeric',
                })}
              </time>
            )}
          </Link>
        </section>
      )}

      {/* ── Start-here cards ──────────────────────────────────────────── */}
      <section className="px-4 pb-20 max-w-5xl mx-auto w-full">
        <h2 className="text-xl font-semibold text-fd-foreground mb-6 text-center">
          Start here
        </h2>
        <Cards>
          <Card
            href="/docs/guide/architecture"
            title="How yoink works"
            description="Drift detection, deploy lock, spec_hash, healthcheck-gated rolling swaps. Read once."
          />
          <Card
            href="/docs/how-to/hetzner-quickstart"
            title="VPS up in 90 seconds"
            description="Hetzner cx23, real Let's Encrypt cert via nip.io, no domain needed."
          />
          <Card
            href="/docs/start/first-deploy"
            title="First deploy in 5 minutes"
            description="Your local repo to a running container on your host — two commands."
          />
        </Cards>
      </section>
    </HomeLayout>
  );
}
