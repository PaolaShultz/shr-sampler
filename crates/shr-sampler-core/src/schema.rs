use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use thiserror::Error;

pub const FORMAT_VERSION: u32 = 1;
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_ZONES: usize = 512;
pub const MAX_SAMPLE_FILES: usize = 256;
pub const MAX_PACKAGE_FILES: usize = MAX_SAMPLE_FILES + 1;
pub const MAX_PACKAGE_DIRECTORIES: usize = 64;
pub const MAX_DECODED_SAMPLE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_SAMPLE_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_POLYPHONY: usize = 64;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentManifest {
    pub format_version: u32,
    pub instrument_id: String,
    pub display_name: String,
    pub metadata: InstrumentMetadata,
    pub max_polyphony: usize,
    pub zones: Vec<ZoneManifest>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentMetadata {
    pub author: String,
    pub source: String,
    pub licence: String,
    pub attribution: String,
    pub notes: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneManifest {
    pub id: String,
    pub sample: SampleRef,
    pub key_low: u8,
    pub key_high: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
    pub root_note: u8,
    pub coarse_transpose: i8,
    pub fine_tune_cents: f32,
    pub gain_db: f32,
    pub pan: f32,
    pub playback_mode: PlaybackMode,
    pub loop_mode: LoopMode,
    pub envelope: Adsr,
    #[serde(default)]
    pub sample_start_frame: u64,
    #[serde(default)]
    pub sample_end_frame: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SampleRef {
    pub path: String,
    pub sha256: String,
    pub metadata: SampleMetadata,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SampleMetadata {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlaybackMode {
    OneShot,
    Gated,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LoopMode {
    None,
    Forward { start_frame: u64, end_frame: u64 },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Adsr {
    pub attack_seconds: f32,
    pub decay_seconds: f32,
    pub sustain_level: f32,
    pub release_seconds: f32,
}

#[derive(Clone, Debug, Error, PartialEq)]
#[error("{message}")]
pub struct ValidationError {
    message: String,
}

impl ValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl InstrumentManifest {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ValidationError> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| ValidationError::new(format!("invalid manifest JSON: {error}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.format_version != FORMAT_VERSION {
            return Err(ValidationError::new(format!(
                "unsupported format version {}; expected {FORMAT_VERSION}",
                self.format_version
            )));
        }
        validate_id(&self.instrument_id, "instrument ID")?;
        validate_text(&self.display_name, "display name", 128)?;
        validate_text(&self.metadata.author, "metadata author", 1024)?;
        validate_text(&self.metadata.source, "metadata source", 4096)?;
        validate_text(&self.metadata.licence, "metadata licence", 1024)?;
        validate_text(&self.metadata.attribution, "metadata attribution", 4096)?;
        validate_text(&self.metadata.notes, "metadata notes", 4096)?;
        if !(1..=MAX_POLYPHONY).contains(&self.max_polyphony) {
            return Err(ValidationError::new("max_polyphony must be 1..=64"));
        }
        if self.zones.is_empty() || self.zones.len() > MAX_ZONES {
            return Err(ValidationError::new(
                "instrument must contain 1..=512 zones",
            ));
        }

        let mut zone_ids = BTreeSet::new();
        let mut samples: BTreeMap<&str, (&str, SampleMetadata)> = BTreeMap::new();
        for zone in &self.zones {
            validate_id(&zone.id, "zone ID")?;
            if !zone_ids.insert(zone.id.as_str()) {
                return Err(ValidationError::new(format!(
                    "duplicate zone ID {}",
                    zone.id
                )));
            }
            validate_relative_path(&zone.sample.path)?;
            if !valid_sha256(&zone.sample.sha256) {
                return Err(ValidationError::new(format!(
                    "{} has an invalid SHA-256 value",
                    zone.id
                )));
            }
            validate_sample_metadata(zone.sample.metadata, &zone.id)?;
            if let Some((hash, metadata)) = samples.get(zone.sample.path.as_str()) {
                if !hash.eq_ignore_ascii_case(&zone.sample.sha256)
                    || *metadata != zone.sample.metadata
                {
                    return Err(ValidationError::new(format!(
                        "{} reuses a sample path with different hash or metadata",
                        zone.id
                    )));
                }
            } else {
                samples.insert(
                    zone.sample.path.as_str(),
                    (&zone.sample.sha256, zone.sample.metadata),
                );
            }
            validate_zone(zone)?;
        }
        if samples.len() > MAX_SAMPLE_FILES {
            return Err(ValidationError::new(
                "instrument exceeds 256 unique sample files",
            ));
        }
        let decoded_bytes = samples.values().try_fold(0u64, |total, (_, metadata)| {
            total
                .checked_add(metadata.frames.saturating_mul(8))
                .ok_or_else(|| ValidationError::new("decoded sample byte count overflow"))
        })?;
        if decoded_bytes > MAX_DECODED_SAMPLE_BYTES {
            return Err(ValidationError::new(
                "instrument exceeds 256 MiB decoded stereo sample memory",
            ));
        }
        Ok(())
    }
}

fn validate_zone(zone: &ZoneManifest) -> Result<(), ValidationError> {
    if zone.key_low > zone.key_high || zone.key_high > 127 {
        return Err(ValidationError::new(format!(
            "{} key range must be within 0..=127",
            zone.id
        )));
    }
    if zone.velocity_low == 0 || zone.velocity_low > zone.velocity_high || zone.velocity_high > 127
    {
        return Err(ValidationError::new(format!(
            "{} velocity range must be within 1..=127",
            zone.id
        )));
    }
    if zone.root_note > 127 || !(-48..=48).contains(&zone.coarse_transpose) {
        return Err(ValidationError::new(format!(
            "{} root note or coarse transpose is out of range",
            zone.id
        )));
    }
    if !finite_range(zone.fine_tune_cents, -1200.0, 1200.0)
        || !finite_range(zone.gain_db, -96.0, 24.0)
        || !finite_range(zone.pan, -1.0, 1.0)
    {
        return Err(ValidationError::new(format!(
            "{} tuning, gain, or pan is non-finite or out of range",
            zone.id
        )));
    }
    for (label, value) in [
        ("attack", zone.envelope.attack_seconds),
        ("decay", zone.envelope.decay_seconds),
        ("release", zone.envelope.release_seconds),
    ] {
        if !finite_range(value, 0.0, 60.0) {
            return Err(ValidationError::new(format!(
                "{} {label} must be finite and 0..=60 seconds",
                zone.id
            )));
        }
    }
    if !finite_range(zone.envelope.sustain_level, 0.0, 1.0) {
        return Err(ValidationError::new(format!(
            "{} sustain level must be finite and 0..=1",
            zone.id
        )));
    }
    let sample_end = zone.sample_end_frame.unwrap_or(zone.sample.metadata.frames);
    if zone.sample_start_frame >= sample_end || sample_end > zone.sample.metadata.frames {
        return Err(ValidationError::new(format!(
            "{} sample start/end frame range is invalid",
            zone.id
        )));
    }
    match zone.loop_mode {
        LoopMode::None => {}
        LoopMode::Forward {
            start_frame,
            end_frame,
        } => {
            if zone.playback_mode == PlaybackMode::OneShot {
                return Err(ValidationError::new(format!(
                    "{} one-shot playback cannot have a forward loop",
                    zone.id
                )));
            }
            if start_frame < zone.sample_start_frame
                || start_frame >= end_frame
                || end_frame > sample_end
            {
                return Err(ValidationError::new(format!(
                    "{} forward loop must lie inside the playable frame range",
                    zone.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_sample_metadata(
    metadata: SampleMetadata,
    zone_id: &str,
) -> Result<(), ValidationError> {
    if !(8_000..=384_000).contains(&metadata.sample_rate)
        || !(1..=2).contains(&metadata.channels)
        || metadata.frames == 0
        || metadata.frames.saturating_mul(8) > MAX_DECODED_SAMPLE_BYTES
    {
        return Err(ValidationError::new(format!(
            "{zone_id} has invalid or excessive decoded sample metadata"
        )));
    }
    Ok(())
}

pub(crate) fn validate_relative_path(value: &str) -> Result<(), ValidationError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 1024
        || value.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ValidationError::new(format!(
            "sample path `{value}` must be a contained relative slash-separated path"
        )));
    }
    Ok(())
}

fn validate_id(value: &str, label: &str) -> Result<(), ValidationError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ValidationError::new(format!(
            "{label} must use 1..=64 lowercase ASCII letters, digits, or hyphens"
        )));
    }
    Ok(())
}

fn validate_text(value: &str, label: &str, maximum: usize) -> Result<(), ValidationError> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(ValidationError::new(format!(
            "{label} must be nonblank text of at most {maximum} UTF-8 bytes without controls"
        )));
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn finite_range(value: f32, minimum: f32, maximum: f32) -> bool {
    value.is_finite() && (minimum..=maximum).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> InstrumentManifest {
        InstrumentManifest {
            format_version: 1,
            instrument_id: "synthetic-piano".into(),
            display_name: "Synthetic Piano".into(),
            metadata: InstrumentMetadata {
                author: "SHR tests".into(),
                source: "Project-authored synthetic fixture".into(),
                licence: "MIT".into(),
                attribution: "None".into(),
                notes: "Test only".into(),
            },
            max_polyphony: 8,
            zones: vec![ZoneManifest {
                id: "zone-001".into(),
                sample: SampleRef {
                    path: "samples/tone.wav".into(),
                    sha256: "0".repeat(64),
                    metadata: SampleMetadata {
                        sample_rate: 48_000,
                        channels: 1,
                        frames: 100,
                    },
                },
                key_low: 0,
                key_high: 127,
                velocity_low: 1,
                velocity_high: 127,
                root_note: 60,
                coarse_transpose: 0,
                fine_tune_cents: 0.0,
                gain_db: 0.0,
                pan: 0.0,
                playback_mode: PlaybackMode::Gated,
                loop_mode: LoopMode::None,
                envelope: Adsr {
                    attack_seconds: 0.0,
                    decay_seconds: 0.0,
                    sustain_level: 1.0,
                    release_seconds: 0.0,
                },
                sample_start_frame: 0,
                sample_end_frame: None,
            }],
        }
    }

    #[test]
    fn strict_json_rejects_unknown_fields() {
        let mut value = serde_json::to_value(manifest()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("future_behavior".into(), true.into());
        assert!(InstrumentManifest::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn duplicate_ids_and_non_finite_metadata_are_rejected() {
        let mut duplicate = manifest();
        duplicate.zones.push(duplicate.zones[0].clone());
        assert!(
            duplicate
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        let mut non_finite = manifest();
        non_finite.zones[0].gain_db = f32::NAN;
        assert!(
            non_finite
                .validate()
                .unwrap_err()
                .to_string()
                .contains("non-finite")
        );
    }

    #[test]
    fn traversal_and_malformed_loops_are_rejected() {
        let mut traversal = manifest();
        traversal.zones[0].sample.path = "../tone.wav".into();
        assert!(traversal.validate().is_err());

        let mut bad_loop = manifest();
        bad_loop.zones[0].loop_mode = LoopMode::Forward {
            start_frame: 80,
            end_frame: 70,
        };
        assert!(bad_loop.validate().is_err());
    }

    #[test]
    fn decoded_memory_limit_is_checked_before_loading() {
        let mut excessive = manifest();
        excessive.zones[0].sample.metadata.frames = MAX_DECODED_SAMPLE_BYTES / 8 + 1;
        assert!(
            excessive
                .validate()
                .unwrap_err()
                .to_string()
                .contains("excessive")
        );
    }
}
