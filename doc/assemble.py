#!/usr/bin/env python3
"""Assemble the documentation site from the per-language page sources.

Pages live in ``doc/<language>/*.html``; English is the source that
translators work from in Weblate, and the other directories are written
back by Weblate. Everything that must stay identical across languages —
the language switcher, ``hreflang`` metadata, the ``lang`` attribute — is
injected here rather than living in the pages, so translators never see
it as a string to translate and a new language needs no edits to any
existing file.

Layout of the generated site::

    site/index.html         English, at the URLs the project has always used
    site/style.css          assets, shared by every language
    site/es/index.html      one directory per additional language

Usage: doc/assemble.py [--source doc] [--out site]
"""

import argparse
import html
import pathlib
import re
import shutil
import sys

# The language pages are written in and translated from.
SOURCE_LANGUAGE = "en"

# Copied to the site root and referenced as ../<asset> from the pages.
ASSETS = ("style.css", "logo.png", "tray.svg")

# Endonyms: a language switcher names each language the way its own
# readers do, so these are deliberately not translated.
LANGUAGE_NAMES = {
    "en": "English",
    "es": "Español",
}

WEBLATE_URL = "https://hosted.weblate.org/engage/starla/"

# Where the site is published. Used for the canonical and alternate link
# metadata, which search engines need as absolute URLs.
SITE_URL = "https://ananthb.github.io/starla"

SWITCHER_ANCHOR = '<div class="meta">'
HEAD_END = "</head>"
HTML_LANG = re.compile(r'<html lang="[^"]*">')


def language_name(language: str) -> str:
    return LANGUAGE_NAMES.get(language, language)


def page_url(language: str, page: str) -> str:
    """Path of a page relative to the site root."""
    return page if language == SOURCE_LANGUAGE else f"{language}/{page}"


def relative_url(from_language: str, to_language: str, page: str) -> str:
    """Link between two languages of the same page."""
    if from_language == to_language:
        return page
    up = "" if from_language == SOURCE_LANGUAGE else "../"
    return up + page_url(to_language, page)


def switcher(current: str, languages: list[str], page: str) -> str:
    links = []
    for language in languages:
        href = relative_url(current, language, page)
        name = html.escape(language_name(language))
        current_attr = ' aria-current="true"' if language == current else ""
        links.append(
            f'<a href="{href}" hreflang="{language}" lang="{language}"'
            f'{current_attr} title="{name}">{language.upper()}</a>'
        )
    links.append(
        f'<a href="{WEBLATE_URL}" title="Translate starla">+</a>'
    )
    return '<span class="langs">' + "".join(links) + "</span>"


def alternates(languages: list[str], page: str) -> str:
    """`hreflang` metadata: which languages this page exists in."""
    links = [
        f'<link rel="alternate" hreflang="{language}" '
        f'href="{SITE_URL}/{page_url(language, page)}">'
        for language in languages
    ]
    links.append(
        f'<link rel="alternate" hreflang="x-default" '
        f'href="{SITE_URL}/{page}">'
    )
    return "\n".join(links) + "\n"


def canonical(language: str, page: str) -> str:
    return f'<link rel="canonical" href="{SITE_URL}/{page_url(language, page)}">\n'


def render(
    source: str,
    language: str,
    content_language: str,
    languages: list[str],
    page: str,
) -> str:
    """Localize one page's chrome and fix its asset paths.

    `language` is the directory the page is published under;
    `content_language` is the language of the prose, which differs when a
    translation does not exist yet and English stands in for it.
    """
    text, count = HTML_LANG.subn(f'<html lang="{content_language}">', source, count=1)
    if count != 1:
        raise SystemExit(f"{language}/{page}: no <html lang=...> to localize")

    if SWITCHER_ANCHOR not in text:
        raise SystemExit(f"{language}/{page}: no {SWITCHER_ANCHOR} to hang the switcher on")
    text = text.replace(
        SWITCHER_ANCHOR,
        SWITCHER_ANCHOR + switcher(language, languages, page),
        1,
    )

    if HEAD_END not in text:
        raise SystemExit(f"{language}/{page}: no </head>")
    # Untranslated pages are a duplicate of the English one, so point
    # search engines at the original rather than at the copy.
    text = text.replace(
        HEAD_END,
        canonical(content_language, page) + alternates(languages, page) + HEAD_END,
        1,
    )

    if language == SOURCE_LANGUAGE:
        # Source pages sit one directory down and reach the shared assets
        # as ../style.css. English is published at the site root, where
        # that would climb out of the site.
        text = text.replace('href="../', 'href="').replace('src="../', 'src="')

    return text


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=pathlib.Path, default=pathlib.Path("doc"))
    parser.add_argument("--out", type=pathlib.Path, default=pathlib.Path("site"))
    args = parser.parse_args()

    languages = sorted(
        path.name
        for path in args.source.iterdir()
        if path.is_dir() and any(path.glob("*.html"))
    )
    if SOURCE_LANGUAGE not in languages:
        raise SystemExit(f"no {SOURCE_LANGUAGE}/ pages in {args.source}")
    # Source language first, then alphabetical: the switcher reads better
    # when it starts from the language everything is translated from.
    languages.remove(SOURCE_LANGUAGE)
    languages.insert(0, SOURCE_LANGUAGE)

    pages = sorted(path.name for path in (args.source / SOURCE_LANGUAGE).glob("*.html"))

    args.out.mkdir(parents=True, exist_ok=True)
    for asset in ASSETS:
        shutil.copy(args.source / asset, args.out / asset)

    for language in languages:
        directory = args.out if language == SOURCE_LANGUAGE else args.out / language
        directory.mkdir(parents=True, exist_ok=True)

        for page in pages:
            source_page = args.source / language / page
            content_language = language
            if not source_page.exists():
                content_language = SOURCE_LANGUAGE
                # A translation Weblate has not written yet: link to the
                # language anyway, but serve the English page there so the
                # switcher never lands on a 404.
                source_page = args.source / SOURCE_LANGUAGE / page
                print(f"{language}/{page}: untranslated, serving English", file=sys.stderr)
            (directory / page).write_text(
                render(
                    source_page.read_text(),
                    language,
                    content_language,
                    languages,
                    page,
                )
            )

    print(f"assembled {len(pages)} pages in {', '.join(languages)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
