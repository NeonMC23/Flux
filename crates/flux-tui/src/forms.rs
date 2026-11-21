//! Structured form display and validation helpers.

use std::path::PathBuf;

use flux_app::{AlbumId, Controller, TrackMetadataEdit};
use flux_core::{ArtistId, Track, TrackId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageEdit {
    Preserve,
    Remove,
    Set(PathBuf),
}

pub(crate) struct ArtistEditForm {
    pub(crate) id: ArtistId,
    pub(crate) name: String,
    pub(crate) aliases: Option<Vec<String>>,
    pub(crate) description: Option<Option<String>>,
    pub(crate) image: ImageEdit,
}

pub(crate) struct AlbumEditForm {
    pub(crate) id: AlbumId,
    pub(crate) title: String,
    pub(crate) release_type: flux_app::ReleaseType,
    pub(crate) cover: ImageEdit,
}

pub(crate) fn album_edit_form(
    id: AlbumId,
    title: &str,
    release_type: flux_app::ReleaseType,
) -> String {
    format!("{id}|{title}|{}|", release_type.as_str())
}

pub(crate) fn parse_artist_edit_form(input: &str) -> Result<ArtistEditForm, String> {
    let fields: Vec<&str> = input.split('|').map(str::trim).collect();
    if !(2..=5).contains(&fields.len()) {
        return Err(
            "Artist edit format: artist-id|name|aliases|description|image-path; aliases are separated by semicolons; empty image preserves and - removes."
                .to_owned(),
        );
    }
    let id = fields[0]
        .parse::<u64>()
        .map(ArtistId::new)
        .map_err(|_| "Artist ID must be a whole number.".to_owned())?;
    if fields[1].is_empty() {
        return Err("Artist name must not be empty.".to_owned());
    }
    let aliases = fields.get(2).map(|value| {
        value
            .split(';')
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    });
    let description = fields
        .get(3)
        .map(|value| (!value.is_empty()).then(|| (*value).to_owned()));
    Ok(ArtistEditForm {
        id,
        name: fields[1].to_owned(),
        aliases,
        description,
        image: ImageEdit::parse(fields.get(4).copied())?,
    })
}

pub(crate) fn artist_edit_form(controller: &Controller, id: ArtistId) -> String {
    let name = controller
        .artist(id)
        .map(|artist| artist.name().to_owned())
        .unwrap_or_default();
    let aliases = controller.artist_aliases(id).join(";");
    let description = controller.artist_description(id).unwrap_or_default();
    format!("{id}|{name}|{aliases}|{description}|")
}

pub(crate) fn parse_release_type(value: &str) -> Result<flux_app::ReleaseType, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "album" => Ok(flux_app::ReleaseType::Album),
        "ep" => Ok(flux_app::ReleaseType::Ep),
        "single" => Ok(flux_app::ReleaseType::Single),
        "compilation" => Ok(flux_app::ReleaseType::Compilation),
        "other" => Ok(flux_app::ReleaseType::Other),
        _ => Err("Release type must be album, ep, single, compilation or other.".to_owned()),
    }
}

pub(crate) fn parse_album_edit_form(input: &str) -> Result<AlbumEditForm, String> {
    let fields: Vec<&str> = input.split('|').map(str::trim).collect();
    if !(3..=4).contains(&fields.len()) {
        return Err(
            "Album edit format: album-id|title|release-type|cover-path; empty cover preserves and - removes."
                .to_owned(),
        );
    }
    let id = fields[0]
        .parse::<u64>()
        .map(AlbumId::new)
        .map_err(|_| "Album ID must be a whole number.".to_owned())?;
    if fields[1].is_empty() {
        return Err("Album title must not be empty.".to_owned());
    }
    Ok(AlbumEditForm {
        id,
        title: fields[1].to_owned(),
        release_type: parse_release_type(fields[2])?,
        cover: ImageEdit::parse(fields.get(3).copied())?,
    })
}

pub(crate) fn track_edit_form(controller: &Controller, id: TrackId, track: &Track) -> String {
    format!(
        "{} | {} | {} | {} | {}",
        track.title(),
        track.album().unwrap_or_default(),
        track
            .track_number()
            .map(|value| value.to_string())
            .unwrap_or_default(),
        track
            .disc_number()
            .map(|value| value.to_string())
            .unwrap_or_default(),
        controller
            .track_artist_id(id)
            .map(|id| id.to_string())
            .unwrap_or_default(),
    )
}

pub(crate) fn parse_track_edit_form(input: &str) -> Result<TrackMetadataEdit, String> {
    let fields: Vec<&str> = input.split('|').map(str::trim).collect();
    if !(5..=6).contains(&fields.len()) {
        return Err(
            "Track edit format: title|album|track-number|disc-number|artist-id|comma-separated-credit-ids (artist and credit IDs may be empty)."
                .to_owned(),
        );
    }
    let number = |value: &str, field: &str| -> Result<Option<u32>, String> {
        if value.is_empty() {
            Ok(None)
        } else {
            value
                .parse::<u32>()
                .map(Some)
                .map_err(|_| format!("{field} must be a whole number."))
        }
    };
    let artist_id = if fields[4].is_empty() {
        None
    } else {
        Some(ArtistId::new(fields[4].parse::<u64>().map_err(|_| {
            "artist ID must be a whole number.".to_owned()
        })?))
    };
    let credited_artist_ids = fields
        .get(5)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .split(',')
                .map(|id| {
                    id.trim()
                        .parse::<u64>()
                        .map(ArtistId::new)
                        .map_err(|_| "credit IDs must be comma-separated whole numbers.".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(TrackMetadataEdit {
        title: (!fields[0].is_empty()).then(|| fields[0].to_owned()),
        album: (!fields[1].is_empty()).then(|| fields[1].to_owned()),
        track_number: number(fields[2], "track number")?,
        disc_number: number(fields[3], "disc number")?,
        artist_id,
        credited_artist_ids,
    })
}
