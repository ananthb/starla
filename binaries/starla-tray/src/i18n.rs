//! Localization for the tray UI.
//!
//! Strings live in `i18n/<language>/starla-tray.ftl` and are embedded in
//! the binary at compile time, so a translated build needs no runtime
//! asset lookup. Call sites use [`i18n_embed_fl::fl`], which resolves
//! message identifiers against the English catalogue while compiling —
//! a typo or a renamed message is a build error, not a missing menu
//! entry at runtime.

use i18n_embed::fluent::{fluent_language_loader, FluentLanguageLoader};
use i18n_embed::unic_langid::LanguageIdentifier;
use i18n_embed::{DesktopLanguageRequester, LanguageLoader, LanguageRequester};
use rust_embed::RustEmbed;
use std::sync::LazyLock;

#[derive(RustEmbed)]
#[folder = "i18n"]
struct Localizations;

pub static LANGUAGE_LOADER: LazyLock<FluentLanguageLoader> = LazyLock::new(|| {
    let loader: FluentLanguageLoader = fluent_language_loader!();
    loader
        .load_fallback_language(&Localizations)
        .expect("English catalogue is embedded in the binary");
    // Fluent brackets every substituted value in U+2068/U+2069 isolation
    // marks so mixed-direction text lays out correctly. Tray menus render
    // those as empty boxes on Windows and on some Linux shells, and the
    // menu never mixes directions within a line, so turn them off.
    loader.set_use_isolating(false);
    loader
});

/// Load the best available catalogue for the user's locale.
///
/// Falls back to English for anything untranslated: Fluent resolves each
/// message individually, so a half-finished translation shows its
/// translated strings and English for the rest.
pub fn init() {
    let requested = requested_languages();
    if let Err(e) = i18n_embed::select(&*LANGUAGE_LOADER, &Localizations, &requested) {
        // Not fatal — the fallback catalogue is already loaded.
        eprintln!("starla-tray: could not load translations: {}", e);
    }
}

/// `STARLA_LANG` overrides the desktop locale, mostly so translators can
/// check their work without changing system settings:
/// `STARLA_LANG=es starla-tray`. Accepts a comma-separated list in
/// preference order.
fn requested_languages() -> Vec<LanguageIdentifier> {
    match std::env::var("STARLA_LANG") {
        Ok(value) => value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|s| match s.parse::<LanguageIdentifier>() {
                Ok(langid) => Some(langid),
                Err(e) => {
                    eprintln!("starla-tray: ignoring STARLA_LANG entry {:?}: {}", s, e);
                    None
                }
            })
            .collect(),
        Err(_) => DesktopLanguageRequester::new().requested_languages(),
    }
}

/// Languages with a catalogue in this build: English first, then the
/// rest alphabetically.
///
/// The order matters. It decides which language wins when several are
/// loaded at once, and it keeps generated output like the desktop entry
/// stable across builds.
pub fn catalogue_languages() -> Vec<LanguageIdentifier> {
    let english = LANGUAGE_LOADER.fallback_language().clone();
    let mut languages = LANGUAGE_LOADER
        .available_languages(&Localizations)
        .unwrap_or_else(|_| vec![english.clone()]);

    languages.sort_by_key(ToString::to_string);
    languages.sort_by_key(|language| *language != english);
    languages
}

/// Load every embedded catalogue, so strings can be rendered in a
/// nominated language rather than the user's.
///
/// This resets which languages the loader considers current — to
/// English first, per [`catalogue_languages`] — so it is for one-shot
/// tooling ([`crate::packaging`]), not for a running tray.
pub fn load_all_catalogues() {
    let languages = catalogue_languages();
    if let Err(e) = LANGUAGE_LOADER.load_languages(&Localizations, &languages) {
        eprintln!("starla-tray: could not load translations: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluent_syntax::ast;
    use std::collections::{BTreeMap, BTreeSet};

    fn catalogue(lang: &LanguageIdentifier) -> String {
        let path = format!("{}/starla-tray.ftl", lang);
        let file = Localizations::get(&path).unwrap_or_else(|| panic!("{} is embedded", path));
        String::from_utf8(file.data.to_vec()).unwrap_or_else(|_| panic!("{} is UTF-8", path))
    }

    /// Message id -> the `$variables` it interpolates.
    fn messages(source: &str, lang: &LanguageIdentifier) -> BTreeMap<String, BTreeSet<String>> {
        let resource = fluent_syntax::parser::parse(source)
            .unwrap_or_else(|(_, errors)| panic!("{} has syntax errors: {:?}", lang, errors));

        resource
            .body
            .iter()
            .filter_map(|entry| match entry {
                ast::Entry::Message(message) => {
                    let mut vars = BTreeSet::new();
                    if let Some(pattern) = &message.value {
                        collect_variables(pattern, &mut vars);
                    }
                    for attribute in &message.attributes {
                        collect_variables(&attribute.value, &mut vars);
                    }
                    Some((message.id.name.to_string(), vars))
                }
                _ => None,
            })
            .collect()
    }

    fn collect_variables(pattern: &ast::Pattern<&str>, out: &mut BTreeSet<String>) {
        for element in &pattern.elements {
            if let ast::PatternElement::Placeable { expression } = element {
                collect_from_expression(expression, out);
            }
        }
    }

    fn collect_from_expression(expression: &ast::Expression<&str>, out: &mut BTreeSet<String>) {
        match expression {
            ast::Expression::Inline(inline) => collect_from_inline(inline, out),
            ast::Expression::Select { selector, variants } => {
                collect_from_inline(selector, out);
                for variant in variants {
                    collect_variables(&variant.value, out);
                }
            }
        }
    }

    fn collect_from_inline(inline: &ast::InlineExpression<&str>, out: &mut BTreeSet<String>) {
        match inline {
            ast::InlineExpression::VariableReference { id } => {
                out.insert(id.name.to_string());
            }
            ast::InlineExpression::Placeable { expression } => {
                collect_from_expression(expression, out)
            }
            ast::InlineExpression::FunctionReference { arguments, .. } => {
                for positional in &arguments.positional {
                    collect_from_inline(positional, out);
                }
                for named in &arguments.named {
                    collect_from_inline(&named.value, out);
                }
            }
            _ => {}
        }
    }

    /// Fluent wraps substituted values in isolation marks unless told
    /// not to, and those show up as boxes in a tray menu. The setting
    /// lives in a lazy initializer, so pin the behaviour it produces.
    #[test]
    fn arguments_render_without_isolation_marks() {
        // Pinned to English rather than whatever the loader happens to
        // have selected: other tests in this binary load every language.
        let english = LANGUAGE_LOADER.select_languages(&[LANGUAGE_LOADER.fallback_language()]);
        let rendered = english.get_args(
            "menu-uptime",
            std::collections::HashMap::from([("uptime", "3h 12m")]),
        );

        assert_eq!(rendered, "Uptime: 3h 12m");
    }

    /// Guards the embed path: a catalogue in the tree but not in
    /// `catalogue_languages()` is a language the shipped binary cannot
    /// serve, and it would make the parity tests below vacuous.
    #[test]
    fn every_embedded_catalogue_is_available() {
        let available: BTreeSet<String> = catalogue_languages()
            .iter()
            .map(ToString::to_string)
            .collect();
        let embedded: BTreeSet<String> = Localizations::iter()
            .filter_map(|path| path.split('/').next().map(ToOwned::to_owned))
            .collect();

        assert_eq!(embedded, available);
        assert!(
            available.contains(&LANGUAGE_LOADER.fallback_language().to_string()),
            "the fallback language has no catalogue"
        );
    }

    /// Every catalogue has to parse, or the tray silently falls back to
    /// English for the whole language.
    #[test]
    fn every_catalogue_parses() {
        for lang in catalogue_languages() {
            messages(&catalogue(&lang), &lang);
        }
    }

    /// Translations may lag behind English — Fluent falls back per
    /// message — but a message the source no longer has is dead weight,
    /// and a stray identifier is usually a Weblate-side rename.
    #[test]
    fn translations_define_no_unknown_messages() {
        let english = LANGUAGE_LOADER.fallback_language().clone();
        let source = messages(&catalogue(&english), &english);

        for lang in catalogue_languages() {
            if lang == english {
                continue;
            }
            for id in messages(&catalogue(&lang), &lang).keys() {
                assert!(
                    source.contains_key(id),
                    "{} defines `{}`, which is not in the English catalogue",
                    lang,
                    id
                );
            }
        }
    }

    /// A translation that drops or invents a variable renders as an
    /// error placeholder at runtime, so catch it here instead.
    #[test]
    fn translations_use_the_same_variables() {
        let english = LANGUAGE_LOADER.fallback_language().clone();
        let source = messages(&catalogue(&english), &english);

        for lang in catalogue_languages() {
            if lang == english {
                continue;
            }
            for (id, vars) in messages(&catalogue(&lang), &lang) {
                let Some(expected) = source.get(&id) else {
                    continue; // reported by
                              // translations_define_no_unknown_messages
                };
                assert_eq!(
                    &vars, expected,
                    "`{}` in {} interpolates {:?}, English uses {:?}",
                    id, lang, vars, expected
                );
            }
        }
    }
}
