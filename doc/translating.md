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

Weblate's "add component from version control" flow detects every
translatable layout in the repository on its own: point it at
<https://github.com/ananthb/starla.git> and it offers the Fluent
catalogue, the README, one entry per documentation page, and the add-on
options — file masks and base files already filled in. Run it once per
layout, naming the components `tray`, `readme`, `docs-index`,
`docs-install`, `docs-architecture`, `docs-protocol`, `docs-verify` and
`addon`; the file table above says which files each one covers.

What discovery gets wrong by default:

- **"Edit base file"** arrives checked. Turn it off everywhere: English is
  changed in git, not in Weblate.
- **Markdown "Extract code blocks"** arrives checked, which offers
  translators the `docker run` lines from the README's quick start. Turn it
  off; unextracted content is copied through verbatim.
- **"Deduplicate identical strings"** (`markdown_merge_duplicates` /
  `html_merge_duplicates`) is off by default. Turn it on so a repeated line
  stays one unit and reordering cannot lose a translation.

Only the first component needs repository credentials; the rest are linked
to it with `weblate://starla/tray` as their repository.

Repository settings, on the `tray` component that owns the checkout:

- **Version control system**: GitHub pull request
- **Repository branch**: `main`
- **Push branch**: `weblate` — Weblate opens a pull request rather than
  pushing to `main`, so translations go through CI like anything else.
- **Push access comes from the GitHub App, not from a key.** Until it is
  connected, Weblate can read the repository but every push fails with
  `could not read Username`, and it locks the affected components until
  the push succeeds. Connect it under *workspace → Operations →
  Code-hosting connections → Connect GitHub account*, granting the
  `hosted-weblate` app access to this repository; it then pushes and
  opens pull requests with installation tokens.

  The alert Weblate raises suggests switching to
  `git@github.com:ananthb/starla.git` and adding its SSH key as a deploy
  key instead. That cannot work here: Hosted Weblate serves one key for
  the whole instance, GitHub requires deploy keys to be globally unique,
  and the key is already registered against someone else's repository —
  `gh repo deploy-key add` fails with `key is already in use`.
- Enable the **Squash Git commits** add-on (one commit per language).
- Add a GitHub webhook to <https://hosted.weblate.org/hooks/github/> so
  Weblate notices English string changes immediately; without it the
  repository is only pulled manually.

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
