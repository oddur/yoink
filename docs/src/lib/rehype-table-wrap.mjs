// Wraps every table in a focusable scroll container so wide reference
// tables scroll inside the page instead of stretching it.
import { visit } from 'unist-util-visit';

export function rehypeTableWrap() {
  return (tree) => {
    visit(tree, 'element', (node, index, parent) => {
      if (node.tagName !== 'table' || !parent || index === undefined) return;
      if (parent.type === 'element' && parent.properties?.className?.includes?.('table-wrap')) return;
      parent.children[index] = {
        type: 'element',
        tagName: 'div',
        properties: { className: ['table-wrap'], tabIndex: 0, role: 'region', ariaLabel: 'Table' },
        children: [node],
      };
    });
  };
}

// Marks short inline code so it never splits across two lines:
// `--tag`, `tag: latest`, `yoink up --build`. Longer code may wrap.
const NOWRAP_MAX = 26;
const text = (node) => (node.type === 'text' ? node.value : (node.children ?? []).map(text).join(''));

export function rehypeInlineCodeNowrap() {
  return (tree) => {
    visit(tree, 'element', (node, _index, parent) => {
      if (node.tagName !== 'code' || parent?.tagName === 'pre') return;
      if (text(node).length > NOWRAP_MAX) return;
      const cls = node.properties.className ?? [];
      node.properties.className = [...(Array.isArray(cls) ? cls : [cls]), 'nw'];
    });
  };
}
