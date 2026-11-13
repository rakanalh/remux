// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { satteri } from '@astrojs/markdown-satteri';
import { osKeys } from './src/plugins/os-keys.mjs';

const detectOs = `(() => {
	let os = null;
	try {
		os = localStorage.getItem('remux-os');
	} catch {}
	if (os !== 'mac' && os !== 'linux') {
		const platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || '';
		os = /mac/i.test(platform) ? 'mac' : 'linux';
	}
	document.documentElement.dataset.os = os;
})();`;

// https://astro.build/config
export default defineConfig({
	site: 'https://rakanalh.github.io',
	base: '/remux',
	markdown: { processor: satteri({ hastPlugins: [osKeys] }) },
	integrations: [
		starlight({
			title: 'Remux',
			description:
				'A client–server terminal multiplexer written in Rust, with first-class remote servers over SSH and cross-machine Views.',
			logo: { src: './src/assets/logo.svg' },
			head: [{ tag: 'script', content: detectOs }],
			customCss: ['./src/styles/os.css'],
			components: { ThemeSelect: './src/components/ThemeSelect.astro' },
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/rakanalh/remux' }],
			editLink: { baseUrl: 'https://github.com/rakanalh/remux/edit/master/site/' },
			sidebar: [
				{
					label: 'Start here',
					items: [
						{ label: 'Overview', slug: 'index' },
						{ label: 'Install', slug: 'install' },
						{ label: 'Quick start', slug: 'quick-start' },
						{ label: 'Concepts', slug: 'concepts' },
						{ label: 'Keyboard', slug: 'keyboard' },
					],
				},
				{
					label: 'Using Remux',
					items: [
						{ label: 'Layouts', slug: 'layouts' },
						{ label: 'Views', slug: 'views' },
						{ label: 'Remotes over SSH', slug: 'remotes' },
						{ label: 'Sidebars', slug: 'sidebars' },
						{ label: 'Agents', slug: 'agents' },
						{ label: 'Agent messaging', slug: 'messaging' },
						{ label: 'Resuming agents', slug: 'session-restore' },
						{ label: 'Sessions & persistence', slug: 'sessions' },
					],
				},
				{
					label: 'Configure',
					items: [
						{ label: 'Configuration', slug: 'configuration' },
						{ label: 'Config reference', slug: 'config-reference' },
						{ label: 'Keybindings', slug: 'keybindings' },
						{ label: 'Theme', slug: 'theme' },
					],
				},
				{
					label: 'Reference',
					items: [{ label: 'CLI reference', slug: 'cli' }],
				},
				{
					label: 'Help',
					items: [{ label: 'Troubleshooting', slug: 'troubleshooting' }],
				},
			],
		}),
	],
});
