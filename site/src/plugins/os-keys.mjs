const ALT_KEY = /^Alt(?:[-+](.+))?$/;
const MARKER = /^<!--\s*os-keys:\s*(off|on)\s*-->$/;

const ARROWS = { Left: '←', Right: '→', Up: '↑', Down: '↓' };

function macForm(key) {
	if (key === undefined) return { text: '⌥ Option' };
	if (/^[A-Z]$/.test(key)) return { text: `⌥⇧${key}`, title: `Option-Shift-${key.toLowerCase()}` };
	const shifted = /^Shift-(.+)$/.exec(key);
	const name = shifted ? shifted[1] : key;
	const shift = shifted ? '⇧' : '';
	const title = `Option-${shifted ? 'Shift-' : ''}${name}`;
	return { text: `⌥${shift}${ARROWS[name] ?? name}`, title };
}

function span(className, text, title) {
	const properties = { className: [className] };
	if (className === 'os-mac') properties['data-pagefind-ignore'] = '';
	if (title) properties.title = title;
	return { type: 'element', tagName: 'span', properties, children: [{ type: 'text', value: text }] };
}

function collectOffRanges(root) {
	const ranges = [];
	let offFrom = null;
	const walk = (node) => {
		if (node.type === 'raw' && node.position) {
			const marker = MARKER.exec(node.value.trim());
			if (marker?.[1] === 'off' && offFrom === null) offFrom = node.position.start.line;
			if (marker?.[1] === 'on' && offFrom !== null) {
				ranges.push([offFrom, node.position.start.line]);
				offFrom = null;
			}
		}
		node.children?.forEach(walk);
	};
	walk(root);
	if (offFrom !== null) ranges.push([offFrom, Infinity]);
	return ranges;
}

/**
 * Renders every inline `Alt-<key>` code span twice, as written and with the macOS Option glyph,
 * so the OS chooser can switch between them with CSS alone and a page without JS still reads
 * correctly. Code blocks are skipped because they hold config, which spells the modifier `Alt-`
 * on every platform; `<!-- os-keys: off -->` … `<!-- os-keys: on -->` exempts prose that
 * documents that spelling. It is a Markdown hast plugin, so it runs on .md pages only, not .mdx.
 */
export const osKeys = {
	name: 'remux-os-keys',
	options: { position: true },
	before(root, ctx) {
		ctx.data.osKeysOff = collectOffRanges(root);
	},
	element: {
		filter: ['code'],
		visit(node, ctx) {
			if (ctx.parent(node)?.tagName === 'pre') return;
			const line = node.position?.start.line;
			const off = ctx.data.osKeysOff ?? [];
			if (line !== undefined && off.some(([from, to]) => line > from && line < to)) return;
			if (node.children.length !== 1 || node.children[0].type !== 'text') return;
			const original = node.children[0].value;
			const match = ALT_KEY.exec(original);
			if (!match) return;
			const mac = macForm(match[1]);
			ctx.removeChildAt(node, 0);
			ctx.appendChild(node, [span('os-linux', original), span('os-mac', mac.text, mac.title)]);
		},
	},
	raw(node, ctx) {
		if (MARKER.test(node.value.trim())) ctx.removeNode(node);
	},
};
