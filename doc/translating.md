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

Components must be created through the GitHub App connection, not by
typing a repository URL. Connect the app first, under *workspace →
Operations → Code-hosting connections → Connect GitHub account*, granting
`hosted-weblate` access to this repository. Then create every component
from *Create component → From connected account → Import*: that flow is
the only one that offers the `GitHub (via Weblate GitHub app)` version
control system, and it is the binding that lets Weblate push. A component
created from a hand-typed URL uses plain `GitHub pull request` instead,
reads the repository fine, and fails every push with `could not read
Username`, locking itself and everything linked to it; the version
control system cannot be changed afterwards, so such a component has to
be recreated.

The import pre-fills the repository and branch, then discovery detects
every translatable layout on its own: the Fluent catalogue, the README,
one entry per documentation page, and the add-on options — file masks and
base files already filled in. Run it once per layout, naming the
components `tray`, `readme`, `docs-index`, `docs-install`,
`docs-architecture`, `docs-protocol`, `docs-verify` and `addon`; the file
table above says which files each one covers.

What discovery gets wrong by default:

- **"Edit base file"** arrives checked. Turn it off everywhere: English is
  changed in git, not in Weblate.
- **"Manage strings"** arrives checked and is invalid for every format
  used here; Weblate only complains when the component is next saved.
- **Markdown "Extract code blocks"** arrives checked, which offers
  translators the `docker run` lines from the README's quick start. Turn it
  off; unextracted content is copied through verbatim.
- **"Deduplicate identical strings"** (`markdown_merge_duplicates` /
  `html_merge_duplicates`) is off by default. Turn it on so a repeated line
  stays one unit and reordering cannot lose a translation.

Only the first component needs the repository; the rest are linked to it
with `weblate://starla/tray` as their repository. Deleting the component
that owns the checkout deletes every component linked to it, so repoint
the links first if it ever has to be replaced.

The HTML components need `safe-html` in **Translation flags**, so a
translator cannot introduce markup the source does not have.

Push settings live on the `tray` component that owns the checkout, and
the app backend fills most of them in: branch `main`, an empty push URL,
and a pull request opened from a fork rather than a push to `main`, so
translations go through CI like anything else.

Weblate's own alert suggests switching to
`git@github.com:ananthb/starla.git` and adding its SSH key as a deploy key
instead. That cannot work here: Hosted Weblate serves one key for the
whole instance, GitHub requires deploy keys to be globally unique, and the
key is already registered against someone else's repository —
`gh repo deploy-key add` fails with `key is already in use`.

Two more things, once the components exist:

- Enable the **Squash Git commits** add-on (one commit per language).
- Add a GitHub webhook to <https://hosted.weblate.org/hooks/github/> so
  Weblate notices English string changes immediately; without it the
  repository is only pulled manually:

  ```bash
  gh api repos/ananthb/starla/hooks -X POST -f name=web \
    -f 'events[]=push' -f 'config[content_type]=json' \
    -f 'config[url]=https://hosted.weblate.org/hooks/github/'
  ```

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
