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
          ],
        },
        {
          label: '設定',
          items: [
            { slug: 'docs/config' },
            { slug: 'docs/prompts' },
            { slug: 'docs/schema' },
          ],
        },
        {
          label: 'ワークフロー',
          items: [
            { slug: 'docs/workflow' },
            { slug: 'docs/agent' },
            { slug: 'docs/lint' },
            { slug: 'docs/testing' },
            { slug: 'docs/eval' },
          ],
        },
        {
          label: 'ツール・拡張',
          items: [
            { slug: 'docs/mcp' },
            { slug: 'docs/compaction' },
            { slug: 'docs/skills' },
            { slug: 'docs/subagents' },
            { slug: 'docs/tools' },
            { slug: 'docs/serve' },
            { slug: 'docs/attachments' },
          ],
        },
        {
          label: '運用・診断',
          items: [
            { slug: 'docs/trace' },
            { slug: 'docs/output' },
            { slug: 'docs/compare' },
            { slug: 'docs/chat' },
            { slug: 'docs/history' },
            { slug: 'docs/troubleshooting' },
          ],
        },
        {
          label: '開発',
          items: [{ slug: 'docs/development' }],
        },
      ],
    }),
  ],
});
