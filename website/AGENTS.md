# Website Guidelines

`website/` is an Astro 7 + Starlight documentation site. The public site is deployed to GitHub Pages at `https://suzumiyaaoba.github.io/lait/`; the landing page is `/lait/` and documentation pages are under `/lait/docs/`.

## Structure

- `astro.config.mjs` configures the Starlight integration, Japanese root locale, sidebar, and GitHub Pages base path.
- `src/content/docs/index.md` is the splash landing page.
- `src/content/docs/docs/` contains the documentation pages, **generated from `../docs/usage/ja/` at the repository root by `scripts/sync-docs.mjs`** and gitignored — never edit files under `src/content/docs/docs/` directly; edit `docs/usage/ja/*.md` and regenerate. `docs/usage/ja/README.md` doubles as the manifest (its bulleted list supplies each page's title/description and the generated `index.md`'s own list); a page body is copied verbatim apart from swapping its `# Title` + back-link header for Starlight frontmatter, rewriting `./slug.md` links to `/lait/docs/slug/`, and converting any GitHub-style `> [!NOTE]` alert into a Starlight `:::note` aside (see the script's comments for the full list of assumptions it checks and raises on).
- `src/content.config.ts` defines Starlight's docs collection.

## Commands

Run these from `website/`:

```sh
pnpm sync-docs         # regenerate src/content/docs/docs/ from ../docs/usage/ja/
pnpm sync-docs:check   # verify it's already in sync, without writing (used in CI)
pnpm dev
pnpm build
pnpm preview
pnpm types:check
```

`pnpm dev`/`pnpm build`/`pnpm types:check` all run `sync-docs` first; `pnpm build` then runs `astro check && astro build`. `pnpm preview` serves the already-built `dist/` and does not regenerate anything.

Use the current Astro and Starlight documentation in `node_modules` or their official sites when changing framework APIs. Keep generated `.astro/` and `dist/` output out of commits, and run the type check and production build after site changes.
