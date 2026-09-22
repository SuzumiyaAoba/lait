import { defineConfig } from "blume";

export default defineConfig({
  title: "lait",
  description: "Lightweight AI Tool (lait) のドキュメント",
  // Docs are mounted under /docs (the repo docs index lives at /docs/), while
  // deployment.base moves the whole site under GitHub Pages' /lait prefix —
  // a page lands at /lait/docs/<slug>. Sidebar items are written as if mounted
  // at root; Blume rewrites them with both prefixes.
  basePath: "/docs",
  deployment: {
    site: "https://suzumiyaaoba.github.io",
    base: "/lait",
  },
  i18n: {
    defaultLocale: "ja",
    locales: [{ code: "ja", label: "日本語" }],
  },
  navigation: {
    // Header GitHub mark. `github` is deliberately left unset: the Edit/Feedback
    // page actions it enables would link to the generated files under website/
    // docs/, not the real sources in docs/usage/ja/.
    repo: "https://github.com/SuzumiyaAoba/lait",
    // Mirrors the sidebar grouping the old Starlight config declared; page
    // routes are content-root-relative (basePath is added by Blume).
    sidebar: [
      {
        label: "ドキュメント",
        items: ["/", "/getting-started"],
      },
      {
        label: "設定",
        items: ["/config", "/prompts", "/schema", "/deps"],
      },
      {
        label: "ワークフロー",
        items: ["/workflow", "/agent", "/lint", "/testing", "/eval"],
      },
      {
        label: "ツール・拡張",
        items: [
          "/mcp",
          "/compaction",
          "/skills",
          "/subagents",
          "/tools",
          "/serve",
          "/attachments",
        ],
      },
      {
        label: "運用・診断",
        items: [
          "/trace",
          "/output",
          "/compare",
          "/chat",
          "/history",
          "/troubleshooting",
        ],
      },
      {
        label: "開発",
        items: ["/development"],
      },
    ],
  },
});
