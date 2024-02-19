use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue},
    middleware::Next,
    response::Response,
};
use i18n_embed::{
    fluent::{fluent_language_loader, FluentLanguageLoader, NegotiationStrategy},
    LanguageLoader,
};
use once_cell::sync::Lazy;
use rust_embed::RustEmbed;
use std::{collections::HashMap, sync::Arc};
use time::OffsetDateTime;

use crate::{state::AppState, user_preferences::UserPreferences};

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

pub static LANGUAGE_DISPLAY_NAMES: Lazy<HashMap<unic_langid::LanguageIdentifier, String>> =
    Lazy::new(|| {
        vec![
            ("en-UK", "English"),
            ("ka-GE", "ქართული"),
            ("bg-BG", "български"),
        ]
        .into_iter()
        .map(|(id, name)| (id.parse().unwrap(), name.to_owned()))
        .collect()
    });

#[derive(Clone, Debug)]
pub struct RequestedLanguages(pub Vec<unic_langid::LanguageIdentifier>);

impl std::fmt::Display for RequestedLanguages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", display_languages(&self.0))
    }
}

pub fn display_languages(languages: &[unic_langid::LanguageIdentifier]) -> String {
    let languages: String = languages
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join(", ");
    format!("[{languages}]")
}

fn parse_accept_language(accept_language: &HeaderValue) -> RequestedLanguages {
    RequestedLanguages(
        accept_language
            .to_str()
            .unwrap_or("")
            .split(',')
            .into_iter()
            .filter_map(|lang| lang.trim().parse::<unic_langid::LanguageIdentifier>().ok())
            .collect(),
    )
}

/// Negotiate which translated string ot use based on the user's requested languages.
pub fn negotiate_translated_string<'a>(
    requested_languages: &[unic_langid::LanguageIdentifier],
    default_language: &'a unic_langid::LanguageIdentifier,
    text: &'a HashMap<unic_langid::LanguageIdentifier, String>,
) -> Option<(&'a unic_langid::LanguageIdentifier, &'a str)> {
    let available_languages: Vec<_> = text.keys().collect();
    let selected = fluent_langneg::negotiate_languages(
        requested_languages,
        &available_languages,
        Some(&default_language),
        fluent_langneg::NegotiationStrategy::Filtering,
    );

    let first = selected.first();

    first.and_then(|first| text.get(first).map(|text| (**first, text.as_str())))
}

pub type I18nLoader = Arc<FluentLanguageLoader>;

pub fn initialize() -> I18nLoader {
    Arc::new(fluent_language_loader!())
}

/// Create an ordered version of [`LANGUAGE_DISPLAY_NAMES`].
pub fn ordered_language_display_names(
    language_order: &[unic_langid::LanguageIdentifier],
) -> Vec<(unic_langid::LanguageIdentifier, String)> {
    order_languages(
        LANGUAGE_DISPLAY_NAMES.clone().into_iter().collect(),
        language_order,
        |(id, _), order_id| id == order_id,
    )
}

/// Order a vec of items according to the order specified in `language_order`, using the `eq` function to
/// match elements in `unordered` to those in `language_order`. Any items which have no match will
/// retain their original order, after any ordered items.
pub fn order_languages<T>(
    mut unordered: Vec<T>,
    language_order: &[unic_langid::LanguageIdentifier],
    eq: impl Fn(&T, &unic_langid::LanguageIdentifier) -> bool,
) -> Vec<T> {
    let mut ordered = Vec::new();
    for l in language_order {
        if let Some(i) = unordered.iter().position(|t| eq(t, l)) {
            ordered.push(unordered.remove(i));
        }
    }
    ordered.extend(unordered.into_iter());
    ordered
}

pub fn load_available_languages<'a>(
    loader: &I18nLoader,
    language_order: &[unic_langid::LanguageIdentifier],
) -> eyre::Result<()> {
    let available_languages = loader.available_languages(&Localizations)?;
    let languages = order_languages(available_languages, language_order, |al, l| al == l);
    let languages_ref: Vec<&unic_langid::LanguageIdentifier> = languages.iter().collect();

    loader.load_languages(&Localizations, &languages_ref)?;

    let languages_display: String = display_languages(&languages);
    tracing::info!("Localizations loaded, languages: {languages_display}");
    Ok(())
}

pub async fn middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let preferences: &UserPreferences = request
        .extensions()
        .get()
        .expect("Expected user_preferences middleware to be installed before this middleware");

    tracing::info!("preferences: {preferences:?}");

    let accept_language = headers.get("Accept-Language").map(parse_accept_language);
    let requested_languages = preferences
        .lang
        .as_ref()
        .map(|lang| {
            let mut requested_languages = RequestedLanguages(vec![lang.clone()]);
            if let Some(accept_language) = &accept_language {
                requested_languages
                    .0
                    .extend(accept_language.0.iter().cloned())
            }
            requested_languages
        })
        .or(accept_language);

    tracing::info!("requested_languages: {requested_languages:?}");

    let loader: I18nLoader = if let Some(requested_languages) = requested_languages {
        let loader = Arc::new(
            state
                .i18n
                .select_languages_negotiate(&requested_languages.0, NegotiationStrategy::Filtering),
        );
        request.extensions_mut().insert(requested_languages);
        loader
    } else {
        state.i18n.clone()
    };

    request.extensions_mut().insert(loader);

    next.run(request).await
}

pub fn format_time(time: OffsetDateTime, i18n: &I18nLoader) -> String {
    let day = time.day();
    let month = time.month() as u8;
    let month_name = i18n.get(&format!("month-{month}"));
    let year = time.year();
    let hour = time.hour();
    let minute = time.minute();
    format!("{day} {month_name} {year} {hour:0>2}:{minute:0>2}")
}
