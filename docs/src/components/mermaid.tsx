import { renderMermaidSVG } from 'beautiful-mermaid';
import { CodeBlock, Pre } from 'fumadocs-ui/components/codeblock';

export function Mermaid({ chart }: { chart: string }) {
  try {
    const svg = renderMermaidSVG(chart, {
      bg: 'var(--color-fd-background)',
      fg: 'var(--color-fd-foreground)',
      transparent: true,
    });
    // Strip fixed pixel width/height so the SVG scales with its container.
    // The viewBox is preserved, so aspect ratio is maintained at any size.
    const responsive = svg.replace(
      /(<svg[^>]*?)\s+width="[^"]*"\s+height="[^"]*"/,
      '$1 style="max-width:100%;height:auto;"',
    );
    return <div dangerouslySetInnerHTML={{ __html: responsive }} />;
  } catch {
    return (
      <CodeBlock title="Mermaid">
        <Pre>{chart}</Pre>
      </CodeBlock>
    );
  }
}
