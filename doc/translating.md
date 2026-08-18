# Translating Starla

Everything a user reads is translatable: the tray app's menu, the README,
this documentation site, and the Home Assistant add-on's configuration
options. Translation happens on Weblate — no Rust, no git, no pull
request.

**<https://hosted.weblate.org/engage/starla/>**

## For translators

Pick your language and start typing. A few things worth knowing:

- **Nothing has to be finished.** Every string falls back to English on
  its own, so a half-translated language ships and improves over time.
- **Placeholders must survive.** `{ $count }`, `{ $uptime }` and friends
  are substituted at runtime; a translation that drops or renames one
  fails CI. Their order is free — put them where the sentence needs them.
- **Product names stay.** "Starla", "RIPE Atlas", "Prometheus", measurement
  type names (`ping`, `traceroute`, `dns`), and anything in `code font`
  are not translated.
- **Try it before it ships.** The tray app takes an override:

  ```bash
  STARLA_LANG=es starla-tray
  ```

- **A new language needs no repository change.** Start it in Weblate and
  it appears in the tray, the docs site's language switcher, and the
  desktop launcher entry on the next release.

## What is translated, and where it lives

| Component | Source of truth | Translations |
| --------- | --------------- | ------------ |
| Tray app | `binaries/starla-tray/i18n/en/starla-tray.ftl` | `binaries/starla-tray/i18n/<lang>/starla-tray.ftl` |
| README | `README.md` | `README.<lang>.md` |
| Documentation site | `doc/en/*.html` | `doc/<lang>/*.html` |
| Home Assistant add-on options | `starla/translations/en.yaml` | `starla/translations/<lang>.yaml` |

Two things are deliberately *not* translated:

- **Log output.** Logs are searched, grepped and pasted into issues;
  keeping them in one language keeps them useful.
- **The `starla` CLI.** Flags and their help are part of the interface
  scripts depend on.
- **The add-on's `DOCS.md`.** Home Assistant serves a single documentation
  page per add-on and has no mechanism for a localized one; only the
  option labels in `translations/` reach the Supervisor UI.

Some user-visible text is generated rather than translated directly:

- `packaging/starla-tray.desktop` is rendered from the tray catalogues by
  `starla-tray --print-desktop-entry`, so the launcher entry is localized
  from the same strings as the menu.
- The docs site's language switcher, `lang` attributes and `hreflang`
  metadata are injected by `doc/assemble.py`, which is why they never
  show up as strings to translate.

## For maintainers

### Changing English strings

Edit the source file for the component. For the tray that also means the
call site: `fl!()` resolves message ids at compile time, so a renamed or
deleted message is a build error rather than a blank menu entry.

After a translation lands, `packaging/starla-tray.desktop` may need
regenerating — `cargo test -p starla-tray` says so if it does:

```bash
cargo run -p starla-tray -- --print-desktop-entry > packaging/starla-tray.desktop
```

New macOS-visible languages also go in `CFBundleLocalizations` in
`packaging/Info.plist`, and their endonym in `LANGUAGE_NAMES` in
`doc/assemble.py`. Both are covered by tests.

### Weblate project setup

The project runs on [Libre hosting](https://weblate.org/hosting/), which
is free for libre-licensed projects; Starla qualifies under AGPL-3.0.
Apply at <https://hosted.weblate.org/create/billing/> and pick the Libre
plan.

Components to create, all in project `starla`:

| Component | File format | File mask | Monolingual base |
| --------- | ----------- | --------- | ---------------- |
| `tray` | Fluent | `binaries/starla-tray/i18n/*/starla-tray.ftl` | `binaries/starla-tray/i18n/en/starla-tray.ftl` |
| `readme` | Markdown file | `README.*.md` | `README.md` |
| `docs-index` | HTML file | `doc/*/index.html` | `doc/en/index.html` |
| `docs-install` | HTML file | `doc/*/install.html` | `doc/en/install.html` |
| `docs-architecture` | HTML file | `doc/*/architecture.html` | `doc/en/architecture.html` |
| `docs-protocol` | HTML file | `doc/*/protocol.html` | `doc/en/protocol.html` |
| `docs-verify` | HTML file | `doc/*/verify.html` | `doc/en/verify.html` |
| `addon` | YAML file | `starla/translations/*.yaml` | `starla/translations/en.yaml` |

For each: source language English, "Template for new translations" set to
the monolingual base file, and "Edit base file" left off — English is
changed in git, not in Weblate. The Markdown and HTML components take the
format parameter `markdown_merge_duplicates` / `html_merge_duplicates` so
a repeated line stays one unit.

Only the first component needs repository credentials; the rest can be
linked to it (`weblate://starla/tray`).

Repository settings:

- **Repository branch**: `main`
- **Push branch**: `weblate` — Weblate opens a pull request rather than
  pushing to `main`, so translations go through CI like anything else.
- Enable the **Squash Git commits** add-on (one commit per language) and
  **Update LINGUAS file** is not needed.
- Add a GitHub webhook to <https://hosted.weblate.org/hooks/github/> so
  Weblate notices English string changes immediately.

`.weblate` in the repository root points the
[`wlc`](https://docs.weblate.org/en/latest/wlc.html) command line client
at the project:

```bash
wlc pull      # make Weblate fetch the latest English strings
wlc commit    # flush pending translations to git
```

### Verifying a translated build

```bash
python3 doc/assemble.py --out site   # docs site, all languages
cargo test -p starla-tray            # catalogue parity, packaging freshness
STARLA_LANG=es cargo run -p starla-tray
```

The `i18n` workflow runs the first two on every pull request that touches
a translatable file.
