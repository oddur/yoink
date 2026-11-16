import { getCollection, type CollectionEntry } from 'astro:content';

type Meta = { title?: string; pages?: string[] };
const metas = import.meta.glob<Meta>('../../content/docs/**/meta.json', {
  eager: true,
  import: 'default',
});

const metaFor = (dir: string): Meta | undefined =>
  metas[`../../content/docs/${dir ? dir + '/' : ''}meta.json`];

export type NavLink = { kind: 'link'; title: string; href: string; id: string; description?: string };
export type NavLabel = { kind: 'label'; title: string };
export type NavSection = { title: string; href?: string; id?: string; items: (NavLink | NavLabel)[] };

// Titles are plain text in navigation: drop Markdown code ticks.
export const plain = (s: string) => s.replace(/`/g, '');

export const docHref = (id: string) => (id === 'index' ? '/docs' : `/docs/${id}`);

export async function getNav() {
  const entries = await getCollection('docs');
  const byId = new Map<string, CollectionEntry<'docs'>>(entries.map((e) => [e.id, e]));
  const link = (id: string): NavLink | undefined => {
    const e = byId.get(id);
    return e && { kind: 'link', title: plain(e.data.title), href: docHref(id), id, description: e.data.description && plain(e.data.description) };
  };

  const sections: NavSection[] = [];
  const loose: NavLink[] = [];
  for (const name of metaFor('')?.pages ?? []) {
    const meta = metaFor(name);
    if (!meta) {
      const l = link(name);
      if (l) loose.push(l);
      continue;
    }
    const items: (NavLink | NavLabel)[] = [];
    for (const p of meta.pages ?? []) {
      const sep = /^---(.+)---$/.exec(p);
      if (sep) items.push({ kind: 'label', title: sep[1] });
      else {
        const l = link(`${name}/${p}`);
        if (l) items.push(l);
      }
    }
    sections.push({
      title: meta.title ?? name,
      href: byId.has(name) ? docHref(name) : undefined,
      id: byId.has(name) ? name : undefined,
      items,
    });
  }
  if (loose.length) sections.push({ title: 'More', items: loose });

  // Reading order for previous / next.
  const order: NavLink[] = [];
  const home = link('index');
  if (home) order.push(home);
  for (const s of sections) {
    if (s.id && s.href) order.push({ kind: 'link', title: s.title, href: s.href, id: s.id });
    for (const i of s.items) if (i.kind === 'link') order.push(i);
  }
  return { sections, order };
}
