#!/usr/bin/env node
// Generates `website/src/content/docs/docs/*.md` from the canonical Japanese
// user documentation in `docs/usage/ja/` at the repository root (see
// `website/AGENTS.md`). This replaces hand-copying the same 15 files into
// two trees, which had already drifted (see the design-review commit that
// introduced this script).
//
// `docs/usage/ja/README.md` doubles as both the doc index and the manifest
// this script parses: each bullet
//   - [Title](./slug.md) — description text
// supplies a page's `title`/`description` frontmatter and its position in
// the generated `index.md`'s own list. Every page body is copied verbatim
// except for:
//   - the leading `# Title` line and the `[ドキュメント目次に戻る](./README.md)`
//     back-link block, both replaced by Starlight frontmatter, and
//   - relative links to another doc page (`./slug.md`, optionally with a
//     `#anchor`), rewritten to this site's absolute `/lait/docs/slug/` path.
//
// Run via `pnpm sync-docs` (or automatically as part of `pnpm dev`/`build`/
// `types:check`, see package.json). `--check` verifies the generated tree
// matches what's committed without writing, for CI to catch a stale copy.

import { readFileSync, writeFileSync, readdirSync, mkdirSync, unlinkSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const websiteDir = path.resolve(fileURLToPath(new URL("..", import.meta.url)));
const repoRoot = path.resolve(websiteDir, "..");
const srcDir = path.join(repoRoot, "docs/usage/ja");
const destDir = path.join(websiteDir, "src/content/docs/docs");

const README_LINK_LINE = "[ドキュメント目次に戻る](./README.md)";

// Not derivable from `docs/usage/ja/README.md` (which has no meta
// description of its own, only per-page bullets) — kept here instead.
const INDEX_DESCRIPTION =
  "lait の使い方ドキュメント一覧。まずは「はじめに」から読むのがおすすめです。";

function readSource(name) {
  return readFileSync(path.join(srcDir, name), "utf8");
}

/** Rewrites `./slug.md`/`./slug.md#anchor`/`./README.md` into this site's
 * absolute `/lait/docs/slug/`(`#anchor`)/`/lait/docs/` paths. */
function rewriteLinks(body) {
  return body.replace(
    /\]\(\.\/([A-Za-z0-9_-]+)\.md(#[^)]*)?\)/g,
    (_match, slug, anchor = "") =>
      slug === "README" ? `](/lait/docs/${anchor})` : `](/lait/docs/${slug}/${anchor})`,
  );
}

// GitHub's alert types down to Starlight's four `Aside` variants (`note`,
// `tip`, `caution`, `danger`) — Starlight doesn't render `> [!NOTE]` blocks
// on its own, so a GFM alert needs converting to its `:::type ... :::`
// syntax to keep the visual callout the site had before generation.
const GFM_ALERT_TO_ASIDE = {
  NOTE: "note",
  TIP: "tip",
  IMPORTANT: "caution",
  WARNING: "caution",
  CAUTION: "danger",
};

/** Converts a GitHub-style alert blockquote (`> [!NOTE]` followed by more
 * `> `-prefixed lines) into a Starlight `:::note ... :::` aside. Only the
 * `[!TYPE]` marker line and the quote-prefixed lines immediately below it
 * are consumed; unrelated blockquotes are left untouched. */
function rewriteGfmAlerts(body) {
  const lines = body.split("\n");
  const out = [];

  for (let i = 0; i < lines.length; i++) {
    const marker = lines[i].match(/^> \[!([A-Z]+)\]$/);
    const aside = marker && GFM_ALERT_TO_ASIDE[marker[1]];
    if (!aside) {
      out.push(lines[i]);
      continue;
    }

    const contentLines = [];
    let j = i + 1;
    while (j < lines.length && lines[j].startsWith("> ")) {
      contentLines.push(lines[j].slice(2));
      j++;
    }
    if (contentLines.length === 0) {
      throw new Error(`GFM alert '${lines[i]}' has no following '> ' content lines to convert`);
    }

    out.push(`:::${aside}`, ...contentLines, ":::");
    i = j - 1;
  }

  return out.join("\n");
}

function frontmatter(title, description) {
  const escape = (value) => value.replaceAll('"', '\\"');
  return `---\ntitle: "${escape(title)}"\ndescription: "${escape(description)}"\n---\n\n`;
}

/** Parses the README's bullet list into an ordered manifest of
 * `{ slug, title, description }`, `description` with backticks stripped
 * (matching the existing convention for the frontmatter field — code spans
 * read fine inline in the page body's own list, but not in an HTML meta
 * attribute). */
function parseManifest(readme) {
  const bulletPattern = /^- \[(.+?)\]\(\.\/(.+?)\.md\) — (.+)$/gm;
  const manifest = [];
  for (const match of readme.matchAll(bulletPattern)) {
    const [, title, slug, descriptionRaw] = match;
    manifest.push({
      slug,
      title,
      description: descriptionRaw.replaceAll("`", ""),
    });
  }
  if (manifest.length === 0) {
    throw new Error("no bullets matched in docs/usage/ja/README.md — is its list format unchanged?");
  }
  return manifest;
}

function buildPage(slug, title) {
  const raw = readSource(`${slug}.md`);
  const lines = raw.split("\n");

  const expectedHeading = `# ${title}`;
  if (lines[0] !== expectedHeading) {
    throw new Error(
      `docs/usage/ja/${slug}.md: expected first line '${expectedHeading}', found '${lines[0]}'`,
    );
  }
  if (lines[1] !== "" || lines[2] !== README_LINK_LINE || lines[3] !== "") {
    throw new Error(
      `docs/usage/ja/${slug}.md: expected the usual '# Title' / blank / back-link / blank header block`,
    );
  }

  const body = lines.slice(4).join("\n");
  return body;
}

function buildIndex(readme) {
  const titleLine = readme.split("\n", 1)[0];
  const title = titleLine.replace(/^# /, "");

  // Everything after the H1 and its blank line is the intro paragraph +
  // bullet list, copied verbatim apart from the link rewrite (the index
  // page's own body keeps the descriptions' backticks, unlike frontmatter).
  const body = readme.split("\n").slice(2).join("\n");
  return { title, body };
}

function generate() {
  const readme = readSource("README.md");
  const manifest = parseManifest(readme);

  const files = new Map();

  const { title: indexTitle, body: indexBody } = buildIndex(readme);
  files.set("index.md", frontmatter(indexTitle, INDEX_DESCRIPTION) + rewriteGfmAlerts(rewriteLinks(indexBody)));

  for (const { slug, title, description } of manifest) {
    const body = buildPage(slug, title);
    files.set(`${slug}.md`, frontmatter(title, description) + rewriteGfmAlerts(rewriteLinks(body)));
  }

  return files;
}

function main() {
  const check = process.argv.includes("--check");
  const files = generate();

  if (check) {
    let existing;
    try {
      existing = new Set(readdirSync(destDir).filter((name) => name.endsWith(".md")));
    } catch {
      existing = new Set();
    }

    const problems = [];
    for (const [name, content] of files) {
      let current;
      try {
        current = readFileSync(path.join(destDir, name), "utf8");
      } catch {
        problems.push(`missing: ${name}`);
        continue;
      }
      if (current !== content) {
        problems.push(`stale: ${name}`);
      }
      existing.delete(name);
    }
    for (const orphan of existing) {
      problems.push(`orphaned (no longer generated): ${orphan}`);
    }

    if (problems.length > 0) {
      console.error("website docs are out of sync with docs/usage/ja/. Run `pnpm sync-docs`:");
      for (const problem of problems) {
        console.error(`  ${problem}`);
      }
      process.exit(1);
    }
    console.log(`${files.size} generated doc page(s) are up to date.`);
    return;
  }

  mkdirSync(destDir, { recursive: true });
  const keep = new Set(files.keys());
  for (const existingName of readdirSync(destDir)) {
    if (existingName.endsWith(".md") && !keep.has(existingName)) {
      console.warn(`sync-docs: removing orphaned ${existingName} (no longer in the manifest)`);
      unlinkSync(path.join(destDir, existingName));
    }
  }
  for (const [name, content] of files) {
    writeFileSync(path.join(destDir, name), content, "utf8");
  }
  console.log(`sync-docs: wrote ${files.size} page(s) to ${path.relative(repoRoot, destDir)}/`);
}

main();
