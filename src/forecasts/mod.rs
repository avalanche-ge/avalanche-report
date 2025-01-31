use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
};

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Extension, Json,
};
use axum_extra::routing::TypedPath;
use eyre::{Context, ContextCompat};
use forecast_spreadsheet::{
    options::AreaDefinition, AreaId, Aspect, AspectElevation, Confidence, Distribution,
    ElevationBandId, Forecaster, HazardRating, HazardRatingKind, ProblemKind, Sensitivity, Size,
    TimeOfDay, Trend,
};
use headers::{ContentType, HeaderMapExt};
use http::{header::CONTENT_TYPE, HeaderValue, StatusCode};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, PrimitiveDateTime};
use time_tz::{Offset, TimeZone};
use tracing::instrument;
use unic_langid::LanguageIdentifier;
use utils::serde::duration_seconds;

use crate::{
    database::Database,
    diagrams,
    error::map_eyre_error,
    google_drive::{self, ListFileMetadata},
    i18n::{self, I18nLoader},
    index::ForecastFileView,
    options::Map,
    state::AppState,
    templates::{render, TemplatesWithContext},
    types,
    user_preferences::UserPreferences,
};

pub mod probability;

use probability::Probability;

#[derive(Clone)]
pub struct ForecastFile {
    pub google_drive_id: String,
    pub last_modified: types::Time,
    pub file_blob: Vec<u8>,
    pub parsed_forecast: Option<forecast_spreadsheet::Forecast>,
    pub schema_version: Option<forecast_spreadsheet::Version>,
}

pub type ForecastSpreadsheetSchema = forecast_spreadsheet::options::Options;

pub static GUDUAURI_FORECAST_SCHEMA: Lazy<ForecastSpreadsheetSchema> =
    Lazy::new(|| serde_json::from_str(include_str!("./schemas/gudauri.0.3.1.json")).unwrap());

#[derive(Serialize, PartialEq, Eq, Clone)]
pub struct ForecastDetails {
    pub area: String,
    #[serde(with = "time::serde::rfc3339")]
    pub time: OffsetDateTime,
    pub forecaster: String,
}

#[derive(Clone, Serialize)]
pub struct ForecastFileDetails {
    pub forecast: ForecastDetails,
    pub language: Option<LanguageIdentifier>,
}

pub fn parse_forecast_name(
    file_name: &str,
    forecast_schema: &ForecastSpreadsheetSchema,
) -> eyre::Result<ForecastFileDetails> {
    parse_forecast_name_impl(
        file_name,
        &forecast_schema.area.map,
        &forecast_schema.area_definitions,
    )
}

fn parse_forecast_name_impl(
    file_name: &str,
    area_name_map: &HashMap<String, AreaId>,
    area_definitions: &IndexMap<AreaId, AreaDefinition>,
) -> eyre::Result<ForecastFileDetails> {
    let mut name_parts = file_name.split('.');
    let details = name_parts
        .next()
        .ok_or_else(|| eyre::eyre!("File name is empty"))?;
    let mut details_split = details.split('_');
    let area = details_split
        .next()
        .ok_or_else(|| eyre::eyre!("No area specified"))?
        .to_owned();

    let area_id = area_name_map
        .get(&area)
        .wrap_err_with(|| format!("Cannot find area id for {area}"))?;
    let tz = area_definitions
        .get(area_id)
        .wrap_err_with(|| format!("Cannot find area definition for {area_id}"))?
        .time_zone;

    let primary_offset = tz.get_offset_primary().to_utc();
    let time_string = details_split
        .next()
        .ok_or_else(|| eyre::eyre!("No time specified"))?;
    let format = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]");
    let guess_time = PrimitiveDateTime::parse(time_string, &format)
        .wrap_err_with(|| format!("Error parsing time {time_string:?}"))?
        .assume_offset(primary_offset);
    let real_offset = tz.get_offset_utc(&guess_time).to_utc();
    let time = guess_time.replace_offset(real_offset);
    let forecaster = details_split
        .next()
        .ok_or_else(|| eyre::eyre!("No forecaster specified"))?
        .to_owned();

    let language = Option::transpose(
        name_parts
            .next()
            .map(|language| language.parse().wrap_err("Unable to parse language")),
    )?;

    let forecast_details = ForecastDetails {
        area,
        time,
        forecaster,
    };

    Ok(ForecastFileDetails {
        forecast: forecast_details,
        language,
    })
}

#[derive(Deserialize, TypedPath)]
#[typed_path("/forecasts/{file_name}")]
pub struct ForecastsFilePath {
    pub file_name: String,
}

pub async fn handler(
    ForecastsFilePath { file_name }: ForecastsFilePath,
    State(state): State<AppState>,
    Extension(database): Extension<Database>,
    Extension(i18n): Extension<I18nLoader>,
    Extension(templates): Extension<TemplatesWithContext>,
    Extension(preferences): Extension<UserPreferences>,
    request: axum::extract::Request,
) -> axum::response::Result<Response> {
    let requested_content_type = request.headers().typed_get::<headers::ContentType>();
    Ok(handler_impl(
        requested_content_type,
        file_name,
        &state.options,
        &state.client,
        &database,
        &templates,
        &i18n,
        &preferences,
        state.forecast_spreadsheet_schema,
    )
    .await
    .map_err(map_eyre_error)?)
}

#[derive(Debug, Serialize, Clone)]
pub struct Forecast {
    pub area: AreaId,
    pub forecaster: Forecaster,
    #[serde(with = "time::serde::rfc3339")]
    pub time: OffsetDateTime,
    #[serde(default)]
    pub recent_observations: HashMap<unic_langid::LanguageIdentifier, String>,
    #[serde(default)]
    pub forecast_changes: HashMap<unic_langid::LanguageIdentifier, String>,
    #[serde(default)]
    pub weather_forecast: HashMap<unic_langid::LanguageIdentifier, String>,
    #[serde(with = "duration_seconds")]
    pub valid_for: time::Duration,
    #[serde(default)]
    pub description: HashMap<unic_langid::LanguageIdentifier, String>,
    pub hazard_ratings: IndexMap<HazardRatingKind, HazardRating>,
    pub avalanche_problems: Vec<AvalancheProblem>,
    pub elevation_bands: IndexMap<ElevationBandId, ElevationRange>,
}

impl Forecast {
    pub fn is_current(&self) -> bool {
        let valid_until = self.time + self.valid_for;

        if time::OffsetDateTime::now_utc() <= valid_until {
            true
        } else {
            false
        }
    }

    pub fn try_new(value: forecast_spreadsheet::Forecast) -> eyre::Result<Self> {
        Ok(Self {
            area: value.area,
            forecaster: value.forecaster,
            time: value.time,
            recent_observations: value.recent_observations,
            forecast_changes: value.forecast_changes,
            weather_forecast: value.weather_forecast,
            valid_for: value.valid_for,
            description: value.description,
            hazard_ratings: value.hazard_ratings,
            avalanche_problems: value
                .avalanche_problems
                .into_iter()
                .map(TryInto::try_into)
                .collect::<eyre::Result<_>>()?,
            elevation_bands: value
                .elevation_bands
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        })
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct ElevationRange {
    pub upper: Option<i64>,
    pub lower: Option<i64>,
}

impl From<forecast_spreadsheet::ElevationRange> for ElevationRange {
    fn from(value: forecast_spreadsheet::ElevationRange) -> Self {
        Self {
            upper: value.upper,
            lower: value.lower,
        }
    }
}

/// An extension of [Forecast] with values that can only be calculated on the Rust side, perhaps
/// will be moved to template functions in the future.
#[derive(Debug, Serialize, Clone)]
pub struct ForecastContext {
    #[serde(flatten)]
    pub forecast: Forecast,
    pub formatted_time: String,
    pub formatted_valid_until: String,
    pub map: Map,
    pub is_current: bool,
    pub external_weather: crate::weather::Context,
}

impl ForecastContext {
    pub fn format(
        forecast: Forecast,
        i18n: &I18nLoader,
        options: &crate::Options,
        preferences: &UserPreferences,
    ) -> Self {
        let formatted_time = i18n::format_time(forecast.time, i18n);
        let valid_until_time = forecast.time + forecast.valid_for;
        let formatted_valid_until = i18n::format_time(valid_until_time, i18n);
        let is_current = forecast.is_current();

        Self {
            forecast,
            formatted_time,
            formatted_valid_until,
            map: options.map.clone(),
            is_current,
            external_weather: crate::weather::Context::new(options, preferences),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct AvalancheProblem {
    pub kind: ProblemKind,
    pub aspect_elevation: IndexMap<ElevationBandId, AspectElevation>,
    // TODO: convert to URL with base
    pub aspect_elevation_chart: String,
    pub confidence: Option<Confidence>,
    pub trend: Option<Trend>,
    pub size: Option<Size>,
    pub distribution: Option<Distribution>,
    pub time_of_day: Option<TimeOfDay>,
    pub sensitivity: Option<Sensitivity>,
    #[serde(default)]
    pub description: HashMap<unic_langid::LanguageIdentifier, String>,
    pub probability: Option<Probability>,
}

fn into_diagram_aspect(aspect: &Aspect) -> diagrams::aspect_elevation::Aspect {
    match aspect {
        Aspect::N => diagrams::aspect_elevation::Aspect::N,
        Aspect::NE => diagrams::aspect_elevation::Aspect::NE,
        Aspect::E => diagrams::aspect_elevation::Aspect::E,
        Aspect::SE => diagrams::aspect_elevation::Aspect::SE,
        Aspect::S => diagrams::aspect_elevation::Aspect::S,
        Aspect::SW => diagrams::aspect_elevation::Aspect::SW,
        Aspect::W => diagrams::aspect_elevation::Aspect::W,
        Aspect::NW => diagrams::aspect_elevation::Aspect::NW,
    }
}

impl TryFrom<forecast_spreadsheet::AvalancheProblem> for AvalancheProblem {
    type Error = eyre::Error;

    fn try_from(value: forecast_spreadsheet::AvalancheProblem) -> eyre::Result<Self> {
        let aspect_elevation = value.aspect_elevation;

        fn map_aspects(
            aspect_elevation: &IndexMap<ElevationBandId, AspectElevation>,
            elevation_band: &ElevationBandId,
        ) -> HashSet<diagrams::aspect_elevation::Aspect> {
            aspect_elevation
                .get(elevation_band)
                .map(|aspect_elevation| {
                    aspect_elevation
                        .aspects
                        .iter()
                        .map(into_diagram_aspect)
                        .collect::<HashSet<_>>()
                })
                .unwrap_or(HashSet::new())
        }

        let query = diagrams::aspect_elevation::AspectElevation {
            high_alpine: map_aspects(&aspect_elevation, &ElevationBandId::from("high-alpine")),
            alpine: map_aspects(&aspect_elevation, &ElevationBandId::from("alpine")),
            sub_alpine: map_aspects(&aspect_elevation, &ElevationBandId::from("sub-alpine")),
            ..diagrams::aspect_elevation::AspectElevation::default()
        }
        .into_query();

        let query_string = serde_urlencoded::to_string(query)?;
        let aspect_elevation_chart = format!("/diagrams/aspect_elevation.svg?{query_string}");

        let probability = value
            .sensitivity
            .zip(value.distribution)
            .map(|(sensitivity, distribution)| Probability::calculate(sensitivity, distribution));
        Ok(Self {
            kind: value.kind,
            aspect_elevation,
            aspect_elevation_chart,
            confidence: value.confidence,
            trend: value.trend,
            size: value.size,
            distribution: value.distribution,
            time_of_day: value.time_of_day,
            sensitivity: value.sensitivity,
            description: value.description,
            probability,
        })
    }
}

#[instrument(level = "error", skip_all)]
async fn handler_impl(
    requested_content_type: Option<ContentType>,
    file_name: String,
    options: &crate::Options,
    client: &reqwest::Client,
    database: &Database,
    templates: &TemplatesWithContext,
    i18n: &I18nLoader,
    preferences: &UserPreferences,
    forecast_schema: &ForecastSpreadsheetSchema,
) -> eyre::Result<Response> {
    let (requested_json, file_name) = {
        let path = std::path::Path::new(&file_name);
        if let Some("json") = path.extension().map(OsStr::to_str).flatten() {
            (
                true,
                path.file_stem()
                    .wrap_err("Expected file {file_name} to have a stem")?
                    .to_str()
                    .wrap_err("Unable to convert file path")?
                    .to_owned(),
            )
        } else {
            (
                requested_content_type
                    .map(|content_type| content_type == ContentType::json())
                    .unwrap_or(false),
                file_name,
            )
        }
    };

    // Check that file exists in published folder, and not attempting to access a file outside
    // that.
    let file_list = google_drive::list_files(
        &options.google_drive.published_folder_id,
        &options.google_drive.api_key,
        client,
    )
    .await?;
    let file_metadata = match google_drive::get_file_in_list(&file_name, &file_list) {
        Some(file_metadata) => file_metadata,
        None => return Ok(StatusCode::NOT_FOUND.into_response()),
    };

    let view = if requested_json {
        ForecastFileView::Json
    } else {
        match file_metadata.mime_type.as_str() {
            "application/pdf" => ForecastFileView::Download,
            "application/vnd.google-apps.spreadsheet" => ForecastFileView::Html,
            unexpected => eyre::bail!("Unsupported file mime type {unexpected}"),
        }
    };

    let requested = match view {
        ForecastFileView::Html | ForecastFileView::Json => RequestedForecastData::Forecast,
        ForecastFileView::Download => RequestedForecastData::File,
    };

    match get_forecast_data(
        &file_metadata,
        requested,
        client,
        database,
        &options.google_drive.api_key,
        forecast_schema,
    )
    .await?
    {
        ForecastData::Forecast(forecast) => match view {
            ForecastFileView::Html => {
                let forecast = Forecast::try_new(forecast)
                    .wrap_err("Error converting forecast into template data")?;
                let formatted_forecast =
                    ForecastContext::format(forecast, &i18n, options, preferences);
                render(&templates.environment, "forecast.html", &formatted_forecast)
            }
            ForecastFileView::Json => Ok(Json(forecast).into_response()),
            _ => unreachable!(),
        },
        ForecastData::File(file_bytes) => {
            let mut response = file_bytes.into_response();
            let header_value = HeaderValue::from_str(&file_metadata.mime_type)?;
            response.headers_mut().insert(CONTENT_TYPE, header_value);
            Ok(response)
        }
    }
}

pub enum RequestedForecastData {
    /// Request the forecast as parsed forecast data. File must be a spreadsheet.
    Forecast,
    /// Request the forecast as a file to download.
    File,
}

pub enum ForecastData {
    Forecast(forecast_spreadsheet::Forecast),
    File(Vec<u8>),
}

/// Get the forecast data for a given file in the published directory.
///
/// WARNING: this does not perform the check whether the specified `file_metadata` is within the
/// published directory.
pub async fn get_forecast_data(
    file_metadata: &ListFileMetadata,
    requested: RequestedForecastData,
    client: &reqwest::Client,
    database: &Database,
    google_drive_api_key: &SecretString,
    forecast_schema: &ForecastSpreadsheetSchema,
) -> eyre::Result<ForecastData> {
    if matches!(requested, RequestedForecastData::Forecast) {
        if !file_metadata.is_google_sheet() {
            eyre::bail!("Unsupported mime type for requested data Forecast: {file_metadata:?}");
        }
    }
    let google_drive_id = file_metadata.id.clone();
    let cached_forecast_file: Option<ForecastFile> = Option::transpose(sqlx::query!(
        r#"SELECT google_drive_id, last_modified as "last_modified: types::Time", file_blob, parsed_forecast as "parsed_forecast: sqlx::types::Json<forecast_spreadsheet::Forecast>", schema_version FROM forecast_files WHERE google_drive_id=$1"#,
        google_drive_id
    ).fetch_optional(database).await?.map(|record| {
            eyre::Ok(ForecastFile {
                google_drive_id: record.google_drive_id,
                last_modified: record.last_modified,
                file_blob: record.file_blob,
                parsed_forecast: record.parsed_forecast.map(|f| f.0),
                schema_version: Option::transpose(record.schema_version.map(|sv| sv.parse()))?

            })
    }))?
        .and_then(|cached_forecast_file| {
            let cached_last_modified: OffsetDateTime = cached_forecast_file.last_modified.into();
            let server_last_modified: &OffsetDateTime = &file_metadata.modified_time;
            tracing::debug!("cached last modified {cached_last_modified}, server last modified {server_last_modified}");
            // This logic is a bit buggy on google's side it seems, sometimes they change document
            // but don't update modified time.
            if cached_last_modified == *server_last_modified {
                Some(cached_forecast_file)
            } else {
                tracing::debug!("Found cached forecast file, but it's outdated");
                None
            }
        });

    let forecast_file: ForecastFile = if let Some(cached_forecast_file) = cached_forecast_file {
        tracing::debug!("Using cached forecast file");
        cached_forecast_file
    } else {
        tracing::debug!("Fetching updated/new forecast file");
        let (forecast_file_bytes, forecast): (
            Vec<u8>,
            Option<(
                forecast_spreadsheet::Forecast,
                forecast_spreadsheet::Version,
            )>,
        ) = match requested {
            RequestedForecastData::Forecast => {
                let file = google_drive::export_file(
                    &file_metadata.id,
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                    google_drive_api_key,
                    client,
                )
                .await?;
                let forecast_file_bytes: Vec<u8> = file.bytes().await?.into();
                let forecast: forecast_spreadsheet::Forecast =
                    forecast_spreadsheet::parse_excel_spreadsheet(
                        &forecast_file_bytes,
                        forecast_schema,
                    )
                    .with_context(|| {
                        format!("Error parsing forecast spreadsheet: {file_metadata:?}")
                    })?;

                (
                    forecast_file_bytes,
                    Some((forecast, forecast_schema.schema_version.clone())),
                )
            }
            RequestedForecastData::File => {
                let file =
                    google_drive::get_file(&file_metadata.id, google_drive_api_key, client).await?;
                (file.bytes().await?.into(), None)
            }
        };
        let forecast_file_db = ForecastFile {
            google_drive_id: file_metadata.id.clone(),
            last_modified: file_metadata.modified_time.clone().into(),
            file_blob: forecast_file_bytes.clone(),
            parsed_forecast: forecast.as_ref().map(|f| f.0.clone()),
            schema_version: forecast.as_ref().map(|f| f.1.clone()),
        };
        let parsed_forecast = forecast_file_db
            .parsed_forecast
            .clone()
            .map(sqlx::types::Json);
        let schema_version = forecast_file_db.schema_version.map(|v| v.to_string());
        tracing::debug!("Updating cached forecast file");
        sqlx::query!(
            "INSERT INTO forecast_files VALUES($1, $2, $3, $4, $5) ON CONFLICT(google_drive_id) DO UPDATE SET last_modified=excluded.last_modified, file_blob=excluded.file_blob, parsed_forecast=excluded.parsed_forecast, schema_version=excluded.schema_version",
            forecast_file_db.google_drive_id,
            forecast_file_db.last_modified,
            forecast_file_db.file_blob,
            parsed_forecast,
            schema_version,
        ).execute(database).await?;

        forecast_file_db
    };

    match requested {
        RequestedForecastData::Forecast => {
            if let Some(forecast) = forecast_file.parsed_forecast {
                if forecast_file.schema_version == Some(forecast_schema.schema_version) {
                    tracing::debug!("Re-using parsed forecast");
                    return Ok(ForecastData::Forecast(forecast));
                } else {
                    tracing::warn!(
                        "Cached forecast schema version {:?} doesn't match current {:?}",
                        forecast_file.schema_version,
                        forecast_schema.schema_version
                    );
                }
            }
            tracing::debug!("Re-parsing forecast");
            let forecast: forecast_spreadsheet::Forecast =
                forecast_spreadsheet::parse_excel_spreadsheet(
                    &forecast_file.file_blob,
                    forecast_schema,
                )
                .with_context(|| {
                    format!("Error parsing forecast spreadsheet: {file_metadata:?}")
                })?;

            tracing::debug!("Updating cached parsed forecast and schema version");

            let parsed_forecast = Some(sqlx::types::Json(forecast.clone()));
            let schema_version = Some(forecast_schema.schema_version.to_string());
            sqlx::query!(
                "UPDATE forecast_files SET parsed_forecast=$1, schema_version=$2 WHERE google_drive_id=$3",
                parsed_forecast,
                schema_version,
                forecast_file.google_drive_id
            ).execute(database).await?;

            Ok(ForecastData::Forecast(forecast))
        }
        RequestedForecastData::File => Ok(ForecastData::File(forecast_file.file_blob)),
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use forecast_spreadsheet::{options::AreaDefinition, AreaId};
    use indexmap::IndexMap;

    use crate::forecasts::GUDUAURI_FORECAST_SCHEMA;

    use super::{parse_forecast_name, parse_forecast_name_impl};

    #[test]
    fn test_parse_forecast_name() {
        let forecast_details = parse_forecast_name(
            "Gudauri_2023-01-24T17:00_LF.en.pdf",
            &GUDUAURI_FORECAST_SCHEMA,
        )
        .unwrap();
        insta::assert_json_snapshot!(forecast_details, @r###"
        {
          "forecast": {
            "area": "Gudauri",
            "time": "2023-01-24T17:00:00+04:00",
            "forecaster": "LF"
          },
          "language": "en"
        }
        "###);
    }

    #[test]
    fn test_parse_forecast_name_pre_dst() {
        let mut area_name_map = HashMap::new();
        let mut area_details = IndexMap::new();
        let area_id: AreaId = "melbourne".to_string().into();

        area_name_map.insert("Melbourne".to_string(), area_id.clone());
        area_details.insert(
            area_id,
            AreaDefinition {
                time_zone: time_tz::timezones::get_by_name("Australia/Melbourne").unwrap(),
            },
        );
        let forecast_details = parse_forecast_name_impl(
            "Melbourne_2023-10-01T01:00_LF.en.pdf",
            &area_name_map,
            &area_details,
        )
        .unwrap();
        insta::assert_json_snapshot!(forecast_details, @r###"
        {
          "forecast": {
            "area": "Melbourne",
            "time": "2023-10-01T01:00:00+10:00",
            "forecaster": "LF"
          },
          "language": "en"
        }
        "###);
    }

    #[test]
    fn test_parse_forecast_name_post_dst() {
        let mut area_name_map = HashMap::new();
        let mut area_details = IndexMap::new();
        let area_id: AreaId = "melbourne".to_string().into();

        area_name_map.insert("Melbourne".to_string(), area_id.clone());
        area_details.insert(
            area_id,
            AreaDefinition {
                time_zone: time_tz::timezones::get_by_name("Australia/Melbourne").unwrap(),
            },
        );
        let forecast_details = parse_forecast_name_impl(
            "Melbourne_2023-10-01T02:00_LF.en.pdf",
            &area_name_map,
            &area_details,
        )
        .unwrap();
        insta::assert_json_snapshot!(forecast_details, @r###"
        {
          "forecast": {
            "area": "Melbourne",
            "time": "2023-10-01T02:00:00+11:00",
            "forecaster": "LF"
          },
          "language": "en"
        }
        "###);
    }
}
