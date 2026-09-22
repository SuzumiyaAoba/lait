# Website Guidelines

`website/` is a [Blume](https://useblume.dev) documentation site (Blume generates and drives a hidden Astro project under `.blume/` — never edit that directory). The public site is deployed to GitHub Pages at `https://suzumiyaaoba.github.io/lait/`; the landing page is `/lait/` and documentation pages are under `/lait/docs/`.

## Structure

- `blume.config.ts` is the whole site definition: `basePath: "/docs"` mounts every content page under `/docs` (so `/` stays free for the landing page), `deployment.base: "/lait"` adds the GitHub Pages prefix, `i18n` selects the built-in Japanese UI strings, and `navigation.sidebar` is an explicit list mirroring the doc groups (sidebar item routes are written content-root-relative — `/getting-started`, not `/docs/getting-started`). `github` is intentionally unset so the Edit/Feedback page actions don't link to generated files; the header repo mark comes from `navigation.repo`.
- `pages/index.astro` is the landing page — a Blume custom page using `PageLayout` (full-width, no sidebar). Use `withBase()` from `blume/components/islands/base-path` for internal links so the `/lait` prefix is applied.
- `docs/` is Blume's content root, **generated from `../docs/usage/ja/` at the repository root by `scripts/sync-docs.mjs`** and gitignored — never edit files under `docs/` directly; edit `docs/usage/ja/*.md` and regenerate. `docs/usage/ja/README.md` doubles as the manifest (its bulleted list supplies each page's title/description and the generated `index.md`'s own list); a page body is copied verbatim apart from swapping its `# Title` + back-link header for Blume frontmatter, rewriting `./slug.md` links to the content-root-relative `/slug` form, and converting any GitHub-style `> [!NOTE]` alert into a `:::type` callout (see the script's comments for the full list of assumptions it checks and raises on). Pages containing a callout are emitted as `.mdx` — directives are MDX-only in Blume — everything else stays `.md`.
- `public/favicon.svg` is picked up automatically (Blume detects favicon files by name).

## Commands

Run these from `website/`:

```sh
pnpm sync-docs         # regenerate docs/ from ../docs/usage/ja/
pnpm sync-docs:check   # verify it's already in sync, without writing (not currently wired into CI —
                       # see scripts/sync-docs.mjs's own comment; `pnpm build` regenerates instead of checking)
pnpm dev
pnpm build             # sync-docs + blume check + blume validate + blume build
pnpm preview           # serves dist/ at http://localhost:4321/lait (deployment.base is applied)
pnpm types:check       # sync-docs + blume check
pnpm doctor            # blume doctor — diagnoses config/content problems
```

`pnpm dev`/`pnpm build`/`pnpm types:check` all run `sync-docs` first. `blume check` type-checks pages; `blume validate` chases every link and anchor and fails on broken ones (broken anchors are warnings). Blume's canonical routes carry no trailing slash — write links as `/slug`, not `/slug/`.

Use the current Blume documentation (`https://useblume.dev/docs`, or the copy under `node_modules/blume/docs/` — the `blume` package drives its own Astro version) when changing framework APIs. Keep generated `.blume/`, `.astro/`, `docs/`, and `dist/` output out of commits, and run the type check and production build after site changes.
