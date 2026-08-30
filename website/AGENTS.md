# Website Guidelines

`website/` is an Astro 7 + Starlight documentation site. The public site is deployed to GitHub Pages at `https://suzumiyaaoba.github.io/lait/`; the landing page is `/lait/` and documentation pages are under `/lait/docs/`.

## Structure

- `astro.config.mjs` configures the Starlight integration, Japanese root locale, sidebar, and GitHub Pages base path.
- `src/content/docs/index.md` is the splash landing page.
- `src/content/docs/docs/` contains the documentation pages. Their source of truth is `../../docs/usage/ja/` at the repository root; preserve the site frontmatter when synchronizing content.
- `src/content.config.ts` defines Starlight's docs collection.

## Commands

Run these from `website/`:

```sh
pnpm dev
pnpm build
pnpm preview
pnpm types:check
```

`pnpm build` runs `astro check && astro build`; `pnpm types:check` runs `astro check`.

Use the current Astro and Starlight documentation in `node_modules` or their official sites when changing framework APIs. Keep generated `.astro/` and `dist/` output out of commits, and run the type check and production build after site changes.
