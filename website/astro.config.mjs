import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://suzumiyaaoba.github.io',
  base: '/lait',
  trailingSlash: 'always',
  integrations: [
    starlight({
      title: 'lait',
      defaultLocale: 'root',
      locales: {
        root: {
          label: '日本語',
          lang: 'ja',
        },
      },
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/SuzumiyaAoba/lait',
        },
      ],
      sidebar: [
        {
          label: 'ドキュメント',
          items: [
            { slug: 'docs', label: 'lait 利用ガイド（日本語）' },
            { slug: 'docs/getting-started' },
            { slug: 'docs/config' },
            { slug: 'docs/workflow' },
            { slug: 'docs/agent' },
            { slug: 'docs/lint' },
            { slug: 'docs/mcp' },
            { slug: 'docs/skills' },
            { slug: 'docs/subagents' },
            { slug: 'docs/output' },
            { slug: 'docs/attachments' },
            { slug: 'docs/chat' },
            { slug: 'docs/prompts' },
            { slug: 'docs/history' },
            { slug: 'docs/troubleshooting' },
            { slug: 'docs/development' },
          ],
        },
      ],
    }),
  ],
});
