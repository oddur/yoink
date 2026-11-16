import type { APIRoute } from 'astro';
import { getNav } from '@/lib/nav';
import { getCollection } from 'astro:content';
import { SITE, DESCRIPTION, BREW } from '@/lib/site';
import { mdPath } from '@/lib/llms';

export const GET: APIRoute = async () => {
  const { sections } = await getNav();
  const byId = new Map((await getCollection('docs')).map((e) => [e.id, e]));
  const line = (id: string) => {
    const e = byId.get(id)!;
    return `- [${e.data.title}](${SITE}${mdPath(id)})${e.data.description ? `: ${e.data.description}` : ''}`;
  };

  const out = [
    '# yoink',
    '',
    `> ${DESCRIPTION}`,
    '',
    'yoink is one binary that connects to hosts over SSH and drives Docker. Its whole interface is the `yoink` CLI and a `yoink.yaml` file in the repo. Every link below is the Markdown source of a docs page.',
    '',
    '## If you are an agent asked to deploy a repo',
    '',
    `1. Install: \`${BREW}\` (other methods: ${SITE}${mdPath('how-to/install')}).`,
    `2. Follow the first deploy: ${SITE}${mdPath('tutorials/first-deploy')}.`,
    '3. Edit `yoink.yaml`, then run `yoink up --dry-run --format=markdown` and show the operator the diff before applying.',
    '4. Apply with `yoink up`. A failed healthcheck-gated swap exits non-zero.',
    '5. Verify with `yoink status`. Roll back with `yoink rollback <service>`.',
    `6. Read the limits first: ${SITE}${mdPath('explanation/ai-agents')}. \`yoink tui\` is for humans, and \`yoink secrets edit\` opens \`$EDITOR\`.`,
    '',
    `The full docs in one file: ${SITE}/llms-full.txt`,
    '',
  ];
  for (const s of sections) {
    out.push(`## ${s.title}`, '');
    if (s.id) out.push(line(s.id));
    for (const i of s.items) {
      if (i.kind === 'label') out.push('', `### ${i.title}`, '');
      else out.push(line(i.id));
    }
    out.push('');
  }
  return new Response(out.join('\n'), { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
