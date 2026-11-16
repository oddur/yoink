// Renders ```mermaid fences to inline SVG at build time, so diagrams
// ship as markup and need no client JavaScript.
import { visit } from 'unist-util-visit';
import { fromHtml } from 'hast-util-from-html';
import { renderMermaidSVG } from 'beautiful-mermaid';

export function remarkMermaid() {
  return (tree, file) => {
    visit(tree, 'code', (node, index, parent) => {
      if (node.lang !== 'mermaid' || !parent || index === undefined) return;
      let svg;
      try {
        svg = renderMermaidSVG(node.value, {
          bg: 'var(--paper)',
          // --text, not --ink: --ink stays near-black in the dark theme
          fg: 'var(--text)',
          transparent: true,
        });
      } catch (err) {
        // Fail the build: a diagram that does not render must not ship as raw source.
        throw new Error(`Mermaid diagram failed to render in ${file.path ?? 'a docs page'}: ${err?.message ?? err}`);
      }
      // The library's stylesheet imports Inter from Google Fonts. Drop
      // the request and set labels in the site's own face.
      svg = svg
        .replace(/@import url\([^)]*\);?\s*/g, '')
        .replace(/font-family:\s*'Inter'[^;]*;/g, 'font-family: inherit;');
      parent.children[index] = {
        type: 'mermaidDiagram',
        data: {
          hName: 'div',
          hProperties: { className: ['mermaid-diagram'] },
          hChildren: fromHtml(svg, { fragment: true }).children,
        },
      };
    });
  };
}

// Headings ending in "[step]" are numbered steps: drop the marker and
// tag the heading so the stylesheet can number it.
export function remarkSteps() {
  return (tree) => {
    visit(tree, 'heading', (node) => {
      const last = node.children.at(-1);
      if (last?.type !== 'text' || !/\s*\[step\]\s*$/.test(last.value)) return;
      last.value = last.value.replace(/\s*\[step\]\s*$/, '');
      node.data = { ...node.data, hProperties: { ...node.data?.hProperties, className: ['step'] } };
    });
  };
}
