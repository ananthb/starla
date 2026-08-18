#!/usr/bin/env python3
"""Check the add-on's option translations against its schema.

Home Assistant looks up every configuration option in
``translations/<language>.yaml`` to label it in the add-on's UI. An option
added to ``config.yaml`` without a matching entry in ``translations/en.yaml``
shows up as a bare key, and a translation with a stale key is dead weight —
neither is something the Supervisor complains about, so check it here.

Usage: starla/check-translations.py [addon-directory]
"""

import pathlib
import sys

try:
    import yaml
except ImportError:  # pragma: no cover - depends on the environment
    sys.exit("PyYAML is required: pip install pyyaml")

SOURCE_LANGUAGE = "en"


def option_names(config: dict) -> set[str]:
    """Option keys the add-on accepts, from its schema."""
    return set(config.get("schema", {}))


def translated_names(translation: dict) -> set[str]:
    return set(translation.get("configuration", {}))


def check(addon: pathlib.Path) -> list[str]:
    config = yaml.safe_load((addon / "config.yaml").read_text())
    options = option_names(config)
    problems = []

    translations = sorted((addon / "translations").glob("*.yaml"))
    if not translations:
        return ["translations/ has no catalogues"]

    for path in translations:
        language = path.stem
        catalogue = yaml.safe_load(path.read_text()) or {}
        names = translated_names(catalogue)

        # English describes every option; other languages may lag behind,
        # and Home Assistant falls back to English for what is missing.
        if language == SOURCE_LANGUAGE:
            for missing in sorted(options - names):
                problems.append(f"{path}: option {missing!r} has no name or description")

        for unknown in sorted(names - options):
            problems.append(f"{path}: {unknown!r} is not an option in config.yaml")

        for option, fields in sorted(catalogue.get("configuration", {}).items()):
            for field in ("name", "description"):
                if not (fields or {}).get(field):
                    problems.append(f"{path}: option {option!r} has no {field}")

    return problems


def main() -> int:
    addon = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "starla")
    problems = check(addon)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        return 1
    print(f"{addon}/translations: option coverage ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
