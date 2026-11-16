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
