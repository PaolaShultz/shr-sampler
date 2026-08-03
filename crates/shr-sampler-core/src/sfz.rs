use crate::package::{inspect_wav_bytes, load_package, read_bounded_regular, sha256_hex};
use crate::schema::{
    Adsr, InstrumentManifest, InstrumentMetadata, LoopMode, MAX_DECODED_SAMPLE_BYTES,
    MAX_MANIFEST_BYTES, MAX_SAMPLE_FILE_BYTES, MAX_SAMPLE_FILES, MAX_ZONES, PlaybackMode,
    SampleRef, ZoneManifest, validate_relative_path,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

const SUPPORTED_OPCODES: &[&str] = &[
    "sample",
    "lokey",
    "hikey",
    "key",
    "lovel",
    "hivel",
    "pitch_keycenter",
    "transpose",
    "tune",
    "volume",
    "pan",
    "offset",
    "end",
    "loop_mode",
    "loop_start",
    "loop_end",
    "ampeg_attack",
    "ampeg_decay",
    "ampeg_sustain",
    "ampeg_release",
];
const HARMLESS_IGNORES: &[&str] = &["global_label", "group_label", "region_label"];
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct ImportMetadata {
    pub instrument_id: String,
    pub display_name: String,
    pub author: String,
    pub source: String,
    pub licence: String,
    pub attribution: String,
    pub notes: String,
    pub max_polyphony: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedOpcode {
    pub opcode: String,
    pub line: usize,
    pub header: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportReport {
    pub zones: usize,
    pub sample_files: usize,
    pub ignored_harmless_opcodes: Vec<String>,
}

#[derive(Debug, Error)]
pub enum SfzError {
    #[error("SFZ input error: {0}")]
    Input(String),
    #[error("SFZ syntax error: {0}")]
    Syntax(String),
    #[error("SFZ contains unsupported sound-changing opcodes:\n{report}")]
    Unsupported { report: String },
    #[error("SFZ profile error: {0}")]
    Profile(String),
    #[error("destination error: {0}")]
    Destination(String),
    #[error("imported native package failed validation: {0}")]
    NativeValidation(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Header {
    Global,
    Group,
    Region,
}

impl Header {
    fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Group => "group",
            Self::Region => "region",
        }
    }
}

#[derive(Clone, Debug)]
struct OpcodeValue {
    value: String,
    line: usize,
    header: Header,
}

type Opcodes = BTreeMap<String, OpcodeValue>;

#[derive(Debug)]
struct ParsedSfz {
    regions: Vec<Opcodes>,
    ignored: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct SourceSample {
    source: PathBuf,
    package_path: String,
    sha256: String,
    metadata: crate::schema::SampleMetadata,
}

pub fn import_sfz(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    metadata: ImportMetadata,
) -> Result<ImportReport, SfzError> {
    let source = source.as_ref();
    let destination = destination.as_ref();
    if source.extension().and_then(|value| value.to_str()) != Some("sfz") {
        return Err(SfzError::Input(format!(
            "input must end in .sfz: {}",
            source.display()
        )));
    }
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|error| SfzError::Input(format!("inspect {}: {error}", source.display())))?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(SfzError::Input(format!(
            "input must be a regular non-symlink file: {}",
            source.display()
        )));
    }
    if destination.extension().and_then(|value| value.to_str()) != Some("shrinst") {
        return Err(SfzError::Destination(format!(
            "output must end in .shrinst: {}",
            destination.display()
        )));
    }
    if destination.exists() {
        return Err(SfzError::Destination(format!(
            "refusing to replace existing output {}",
            destination.display()
        )));
    }
    let bytes =
        read_bounded_regular(source, MAX_MANIFEST_BYTES, "SFZ file").map_err(SfzError::Input)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| SfzError::Input("SFZ file is not strict UTF-8".into()))?;
    let parsed = parse_sfz(text)?;
    if parsed.regions.is_empty() {
        return Err(SfzError::Profile("SFZ contains no regions".into()));
    }
    if parsed.regions.len() > MAX_ZONES {
        return Err(SfzError::Profile(format!(
            "SFZ exceeds {MAX_ZONES} regions"
        )));
    }

    let source_parent = source
        .parent()
        .ok_or_else(|| SfzError::Input("SFZ path has no parent directory".into()))?;
    let source_root = fs::canonicalize(source_parent).map_err(|error| {
        SfzError::Input(format!("canonicalize {}: {error}", source_parent.display()))
    })?;
    let mut source_samples: BTreeMap<PathBuf, SourceSample> = BTreeMap::new();
    let mut zones = Vec::with_capacity(parsed.regions.len());
    for (index, opcodes) in parsed.regions.iter().enumerate() {
        let sample_value = required(opcodes, "sample", index)?;
        let normalized_sample = sample_value.replace('\\', "/");
        validate_relative_path(&normalized_sample).map_err(|error| {
            SfzError::Profile(format!("region {} sample path: {error}", index + 1))
        })?;
        if Path::new(&normalized_sample)
            .extension()
            .and_then(|value| value.to_str())
            .is_none_or(|value| !value.eq_ignore_ascii_case("wav"))
        {
            return Err(SfzError::Profile(format!(
                "region {} sample must be WAV; ConvertWithMoss FLAC output is not in this profile",
                index + 1
            )));
        }
        let sample_source = source_root.join(&normalized_sample);
        verify_source_sample(&source_root, &normalized_sample, &sample_source)?;
        let canonical_sample = fs::canonicalize(&sample_source).map_err(|error| {
            SfzError::Input(format!("open sample {}: {error}", sample_source.display()))
        })?;
        if !source_samples.contains_key(&canonical_sample) {
            if source_samples.len() == MAX_SAMPLE_FILES {
                return Err(SfzError::Profile(format!(
                    "SFZ exceeds {MAX_SAMPLE_FILES} unique samples"
                )));
            }
            let sample_bytes =
                read_bounded_regular(&canonical_sample, MAX_SAMPLE_FILE_BYTES, "SFZ sample")
                    .map_err(SfzError::Input)?;
            let sample_metadata =
                inspect_wav_bytes(&normalized_sample, &sample_bytes).map_err(SfzError::Profile)?;
            let package_path = format!("samples/{:04}.wav", source_samples.len() + 1);
            source_samples.insert(
                canonical_sample.clone(),
                SourceSample {
                    source: canonical_sample.clone(),
                    package_path,
                    sha256: sha256_hex(&sample_bytes),
                    metadata: sample_metadata,
                },
            );
        }
        let sample = &source_samples[&canonical_sample];
        zones.push(region_to_zone(index, opcodes, sample)?);
    }
    let decoded_bytes = source_samples.values().try_fold(0u64, |total, sample| {
        total.checked_add(sample.metadata.frames.saturating_mul(8))
    });
    if decoded_bytes.is_none_or(|bytes| bytes > MAX_DECODED_SAMPLE_BYTES) {
        return Err(SfzError::Profile(
            "SFZ exceeds 256 MiB decoded stereo sample memory".into(),
        ));
    }

    let manifest = InstrumentManifest {
        format_version: 1,
        instrument_id: metadata.instrument_id,
        display_name: metadata.display_name,
        metadata: InstrumentMetadata {
            author: metadata.author,
            source: metadata.source,
            licence: metadata.licence,
            attribution: metadata.attribution,
            notes: metadata.notes,
        },
        max_polyphony: metadata.max_polyphony,
        zones,
    };
    manifest
        .validate()
        .map_err(|error| SfzError::Profile(error.to_string()))?;
    write_transaction(destination, &manifest, source_samples.values())?;
    Ok(ImportReport {
        zones: manifest.zones.len(),
        sample_files: source_samples.len(),
        ignored_harmless_opcodes: parsed.ignored.into_iter().collect(),
    })
}

fn parse_sfz(text: &str) -> Result<ParsedSfz, SfzError> {
    let mut current_header = None;
    let mut global = Opcodes::new();
    let mut group = Opcodes::new();
    let mut region = Opcodes::new();
    let mut regions = Vec::new();
    let mut unsupported = Vec::new();
    let mut ignored = BTreeSet::new();

    for (zero_line, raw_line) in text.lines().enumerate() {
        let line_number = zero_line + 1;
        let line = raw_line
            .split_once("//")
            .map_or(raw_line, |(before, _)| before);
        let mut remaining = line.trim();
        if remaining.is_empty() {
            continue;
        }
        if remaining.starts_with('#') || remaining.contains("/*") || remaining.contains("*/") {
            return Err(SfzError::Syntax(format!(
                "line {line_number}: preprocessor directives and block comments are outside the profile"
            )));
        }
        if remaining.starts_with('<') {
            let close = remaining.find('>').ok_or_else(|| {
                SfzError::Syntax(format!("line {line_number}: unterminated header"))
            })?;
            if current_header == Some(Header::Region) {
                finalize_region(&global, &group, &mut region, &mut regions)?;
            }
            let name = &remaining[1..close];
            current_header = Some(match name {
                "global" => {
                    group.clear();
                    Header::Global
                }
                "group" => {
                    group.clear();
                    Header::Group
                }
                "region" => Header::Region,
                _ => {
                    return Err(SfzError::Syntax(format!(
                        "line {line_number}: unsupported header <{name}>"
                    )));
                }
            });
            remaining = remaining[close + 1..].trim();
            if remaining.starts_with('<') {
                return Err(SfzError::Syntax(format!(
                    "line {line_number}: only one header per line is accepted"
                )));
            }
        }
        if remaining.is_empty() {
            continue;
        }
        let header = current_header.ok_or_else(|| {
            SfzError::Syntax(format!(
                "line {line_number}: opcode appears before a header"
            ))
        })?;
        for (opcode, value) in parse_assignments(remaining, line_number)? {
            if HARMLESS_IGNORES.contains(&opcode.as_str()) {
                ignored.insert(opcode.clone());
            } else if !SUPPORTED_OPCODES.contains(&opcode.as_str()) {
                unsupported.push(UnsupportedOpcode {
                    opcode: opcode.clone(),
                    line: line_number,
                    header: header.name().into(),
                });
            }
            let target = match header {
                Header::Global => &mut global,
                Header::Group => &mut group,
                Header::Region => &mut region,
            };
            if target.contains_key(&opcode) {
                return Err(SfzError::Syntax(format!(
                    "line {line_number}: duplicate `{opcode}` in one <{}> scope",
                    header.name()
                )));
            }
            target.insert(
                opcode,
                OpcodeValue {
                    value,
                    line: line_number,
                    header,
                },
            );
        }
    }
    if current_header == Some(Header::Region) {
        finalize_region(&global, &group, &mut region, &mut regions)?;
    }
    if !unsupported.is_empty() {
        let report = unsupported
            .iter()
            .map(|item| format!("- line {} <{}>: {}", item.line, item.header, item.opcode))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(SfzError::Unsupported { report });
    }
    Ok(ParsedSfz { regions, ignored })
}

fn finalize_region(
    global: &Opcodes,
    group: &Opcodes,
    region: &mut Opcodes,
    regions: &mut Vec<Opcodes>,
) -> Result<(), SfzError> {
    if region.is_empty() {
        return Err(SfzError::Syntax("empty <region>".into()));
    }
    let mut effective = global.clone();
    effective.extend(group.clone());
    effective.append(region);
    regions.push(effective);
    Ok(())
}

fn parse_assignments(text: &str, line: usize) -> Result<Vec<(String, String)>, SfzError> {
    let bytes = text.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }
        let key_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if cursor == key_start || bytes.get(cursor) != Some(&b'=') {
            return Err(SfzError::Syntax(format!(
                "line {line}: expected lowercase opcode=value"
            )));
        }
        let opcode = &text[key_start..cursor];
        if opcode.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(SfzError::Syntax(format!(
                "line {line}: opcode `{opcode}` must be lowercase"
            )));
        }
        cursor += 1;
        let value_start = cursor;
        let mut value_end = bytes.len();
        while cursor < bytes.len() {
            if bytes[cursor].is_ascii_whitespace() {
                let whitespace = cursor;
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                let possible_key = cursor;
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
                {
                    cursor += 1;
                }
                if cursor > possible_key && bytes.get(cursor) == Some(&b'=') {
                    value_end = whitespace;
                    cursor = possible_key;
                    break;
                }
            } else {
                cursor += 1;
            }
        }
        let value = text[value_start..value_end].trim();
        if value.is_empty() {
            return Err(SfzError::Syntax(format!(
                "line {line}: `{opcode}` has an empty value"
            )));
        }
        output.push((opcode.into(), value.into()));
    }
    Ok(output)
}

fn region_to_zone(
    index: usize,
    opcodes: &Opcodes,
    sample: &SourceSample,
) -> Result<ZoneManifest, SfzError> {
    if opcodes.contains_key("key")
        && ["lokey", "hikey", "pitch_keycenter"]
            .iter()
            .any(|opcode| opcodes.contains_key(*opcode))
    {
        return Err(profile(
            index,
            "`key` cannot be combined with lokey, hikey, or pitch_keycenter",
        ));
    }
    let key = optional_u8(opcodes, "key", 0, 127, index)?;
    let key_low = key.unwrap_or(optional_u8(opcodes, "lokey", 0, 127, index)?.unwrap_or(0));
    let key_high = key.unwrap_or(optional_u8(opcodes, "hikey", 0, 127, index)?.unwrap_or(127));
    let root_note =
        key.unwrap_or(optional_u8(opcodes, "pitch_keycenter", 0, 127, index)?.unwrap_or(60));
    let velocity_low = optional_u8(opcodes, "lovel", 1, 127, index)?.unwrap_or(1);
    let velocity_high = optional_u8(opcodes, "hivel", 1, 127, index)?.unwrap_or(127);
    let coarse_transpose = optional_i64(opcodes, "transpose", -48, 48, index)?.unwrap_or(0) as i8;
    let fine_tune_cents = optional_f32(opcodes, "tune", -1200.0, 1200.0, index)?.unwrap_or(0.0);
    let gain_db = optional_f32(opcodes, "volume", -96.0, 24.0, index)?.unwrap_or(0.0);
    let pan_percent = optional_f32(opcodes, "pan", -100.0, 100.0, index)?.unwrap_or(0.0);
    let sample_start_frame = optional_u64(opcodes, "offset", index)?.unwrap_or(0);
    let sfz_end = optional_u64(opcodes, "end", index)?;
    let sample_end_frame = sfz_end
        .map(|end| {
            end.checked_add(1)
                .ok_or_else(|| profile(index, "end frame overflows"))
        })
        .transpose()?;
    let loop_mode_value = opcodes
        .get("loop_mode")
        .map(|value| value.value.as_str())
        .unwrap_or("no_loop");
    let loop_start = optional_u64(opcodes, "loop_start", index)?;
    let sfz_loop_end = optional_u64(opcodes, "loop_end", index)?;
    let (playback_mode, loop_mode) = match loop_mode_value {
        "no_loop" => {
            if loop_start.is_some() || sfz_loop_end.is_some() {
                return Err(profile(
                    index,
                    "no_loop cannot carry loop_start or loop_end",
                ));
            }
            (PlaybackMode::Gated, LoopMode::None)
        }
        "one_shot" => {
            if loop_start.is_some() || sfz_loop_end.is_some() {
                return Err(profile(index, "one_shot cannot carry loop points"));
            }
            (PlaybackMode::OneShot, LoopMode::None)
        }
        "loop_sustain" => {
            let start_frame =
                loop_start.ok_or_else(|| profile(index, "loop_sustain needs loop_start"))?;
            let inclusive_end =
                sfz_loop_end.ok_or_else(|| profile(index, "loop_sustain needs loop_end"))?;
            let end_frame = inclusive_end
                .checked_add(1)
                .ok_or_else(|| profile(index, "loop_end overflows"))?;
            (
                PlaybackMode::Gated,
                LoopMode::Forward {
                    start_frame,
                    end_frame,
                },
            )
        }
        "loop_continuous" => {
            return Err(profile(
                index,
                "loop_continuous is not representable; use loop_sustain in the controlled profile",
            ));
        }
        other => return Err(profile(index, &format!("unsupported loop_mode `{other}`"))),
    };
    let envelope = Adsr {
        attack_seconds: optional_f32(opcodes, "ampeg_attack", 0.0, 60.0, index)?.unwrap_or(0.0),
        decay_seconds: optional_f32(opcodes, "ampeg_decay", 0.0, 60.0, index)?.unwrap_or(0.0),
        sustain_level: optional_f32(opcodes, "ampeg_sustain", 0.0, 100.0, index)?.unwrap_or(100.0)
            / 100.0,
        release_seconds: optional_f32(opcodes, "ampeg_release", 0.0, 60.0, index)?.unwrap_or(0.0),
    };
    Ok(ZoneManifest {
        id: format!("zone-{:03}", index + 1),
        sample: SampleRef {
            path: sample.package_path.clone(),
            sha256: sample.sha256.clone(),
            metadata: sample.metadata,
        },
        key_low,
        key_high,
        velocity_low,
        velocity_high,
        root_note,
        coarse_transpose,
        fine_tune_cents,
        gain_db,
        pan: pan_percent / 100.0,
        playback_mode,
        loop_mode,
        envelope,
        sample_start_frame,
        sample_end_frame,
    })
}

fn required<'a>(opcodes: &'a Opcodes, name: &str, region: usize) -> Result<&'a str, SfzError> {
    opcodes
        .get(name)
        .map(|value| value.value.as_str())
        .ok_or_else(|| profile(region, &format!("missing required `{name}`")))
}

fn optional_u8(
    opcodes: &Opcodes,
    name: &str,
    minimum: u8,
    maximum: u8,
    region: usize,
) -> Result<Option<u8>, SfzError> {
    optional_i64(
        opcodes,
        name,
        i64::from(minimum),
        i64::from(maximum),
        region,
    )
    .map(|value| value.map(|value| value as u8))
}

fn optional_u64(opcodes: &Opcodes, name: &str, region: usize) -> Result<Option<u64>, SfzError> {
    let Some(value) = opcodes.get(name) else {
        return Ok(None);
    };
    value.value.parse::<u64>().map(Some).map_err(|_| {
        profile_at(
            region,
            name,
            value,
            "must be a non-negative integer frame index",
        )
    })
}

fn optional_i64(
    opcodes: &Opcodes,
    name: &str,
    minimum: i64,
    maximum: i64,
    region: usize,
) -> Result<Option<i64>, SfzError> {
    let Some(value) = opcodes.get(name) else {
        return Ok(None);
    };
    let parsed = value
        .value
        .parse::<i64>()
        .map_err(|_| profile_at(region, name, value, "must be an integer"))?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(profile_at(
            region,
            name,
            value,
            &format!("must be {minimum}..={maximum}"),
        ));
    }
    Ok(Some(parsed))
}

fn optional_f32(
    opcodes: &Opcodes,
    name: &str,
    minimum: f32,
    maximum: f32,
    region: usize,
) -> Result<Option<f32>, SfzError> {
    let Some(value) = opcodes.get(name) else {
        return Ok(None);
    };
    let parsed = value
        .value
        .parse::<f32>()
        .map_err(|_| profile_at(region, name, value, "must be a number"))?;
    if !parsed.is_finite() || !(minimum..=maximum).contains(&parsed) {
        return Err(profile_at(
            region,
            name,
            value,
            &format!("must be finite and {minimum}..={maximum}"),
        ));
    }
    Ok(Some(parsed))
}

fn profile(region: usize, message: &str) -> SfzError {
    SfzError::Profile(format!("region {}: {message}", region + 1))
}

fn profile_at(region: usize, name: &str, value: &OpcodeValue, message: &str) -> SfzError {
    SfzError::Profile(format!(
        "region {} inherited `{name}` from line {} <{}>: {message}",
        region + 1,
        value.line,
        value.header.name()
    ))
}

fn verify_source_sample(root: &Path, relative: &str, path: &Path) -> Result<(), SfzError> {
    let mut current = root.to_path_buf();
    for component in Path::new(relative).components() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            SfzError::Input(format!(
                "inspect sample path {}: {error}",
                current.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(SfzError::Input(format!(
                "sample paths may not contain symlinks: {}",
                current.display()
            )));
        }
    }
    let canonical = fs::canonicalize(path)
        .map_err(|error| SfzError::Input(format!("open sample {}: {error}", path.display())))?;
    if !canonical.starts_with(root) {
        return Err(SfzError::Input(format!(
            "sample resolves outside the SFZ directory: {relative}"
        )));
    }
    Ok(())
}

fn write_transaction<'a>(
    destination: &Path,
    manifest: &InstrumentManifest,
    samples: impl Iterator<Item = &'a SourceSample>,
) -> Result<(), SfzError> {
    let parent = destination
        .parent()
        .ok_or_else(|| SfzError::Destination("output has no parent directory".into()))?;
    fs::create_dir_all(parent).map_err(|error| {
        SfzError::Destination(format!(
            "create output parent {}: {error}",
            parent.display()
        ))
    })?;
    let filename = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| SfzError::Destination("output filename is not UTF-8".into()))?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{filename}.import-{}-{counter}.shrinst",
        std::process::id()
    ));
    if temporary.exists() {
        return Err(SfzError::Destination(format!(
            "temporary output already exists: {}",
            temporary.display()
        )));
    }
    let result = (|| {
        fs::create_dir(&temporary).map_err(|error| {
            SfzError::Destination(format!("create {}: {error}", temporary.display()))
        })?;
        fs::create_dir(temporary.join("samples")).map_err(|error| {
            SfzError::Destination(format!("create temporary samples directory: {error}"))
        })?;
        for sample in samples {
            fs::copy(&sample.source, temporary.join(&sample.package_path)).map_err(|error| {
                SfzError::Destination(format!("copy sample {}: {error}", sample.source.display()))
            })?;
        }
        let mut manifest_bytes = serde_json::to_vec_pretty(manifest)
            .map_err(|error| SfzError::Destination(format!("serialize manifest: {error}")))?;
        manifest_bytes.push(b'\n');
        if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(SfzError::Destination(
                "generated manifest exceeds 1 MiB".into(),
            ));
        }
        fs::write(temporary.join("manifest.json"), manifest_bytes)
            .map_err(|error| SfzError::Destination(format!("write manifest: {error}")))?;
        load_package(&temporary).map_err(SfzError::NativeValidation)?;
        fs::rename(&temporary, destination).map_err(|error| {
            SfzError::Destination(format!(
                "publish {} to {}: {error}",
                temporary.display(),
                destination.display()
            ))
        })?;
        Ok(())
    })();
    if result.is_err() && temporary.exists() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};

    fn write_synthetic_wav(path: &Path) {
        let mut writer = WavWriter::create(
            path,
            WavSpec {
                channels: 1,
                sample_rate: 48_000,
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
            },
        )
        .unwrap();
        for index in 0..64 {
            writer.write_sample((index * 300 - 9_000) as i16).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn metadata() -> ImportMetadata {
        ImportMetadata {
            instrument_id: "import-fixture".into(),
            display_name: "Import Fixture".into(),
            author: "SHR tests".into(),
            source: "Project-authored controlled SFZ".into(),
            licence: "MIT".into(),
            attribution: "None".into(),
            notes: "Synthetic only".into(),
            max_polyphony: 8,
        }
    }

    #[test]
    fn controlled_profile_imports_keys_velocity_tuning_pan_loop_and_adsr() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("fixture.sfz");
        let output = directory.path().join("fixture.shrinst");
        write_synthetic_wav(&directory.path().join("tone.wav"));
        fs::write(
            &source,
            include_str!("../../../tests/fixtures/controlled-profile.sfz"),
        )
        .unwrap();
        let report = import_sfz(&source, &output, metadata()).unwrap();
        assert_eq!(report.zones, 2);
        assert_eq!(report.sample_files, 1);
        assert_eq!(
            report.ignored_harmless_opcodes,
            vec!["global_label", "group_label"]
        );
        let prepared = load_package(&output).unwrap();
        let manifest = prepared.manifest();
        assert_eq!(manifest.zones[0].key_low, 36);
        assert_eq!(manifest.zones[0].key_high, 60);
        assert_eq!(manifest.zones[0].velocity_low, 1);
        assert_eq!(manifest.zones[0].velocity_high, 80);
        assert_eq!(manifest.zones[0].root_note, 48);
        assert_eq!(manifest.zones[0].coarse_transpose, 1);
        assert_eq!(manifest.zones[0].fine_tune_cents, -7.0);
        assert_eq!(manifest.zones[0].pan, -0.25);
        assert_eq!(manifest.zones[0].sample_start_frame, 2);
        assert_eq!(manifest.zones[0].sample_end_frame, Some(61));
        assert_eq!(
            manifest.zones[0].loop_mode,
            LoopMode::Forward {
                start_frame: 10,
                end_frame: 41
            }
        );
        assert_eq!(manifest.zones[0].envelope.sustain_level, 0.65);
        assert_eq!(manifest.zones[1].key_low, 72);
        assert_eq!(manifest.zones[1].key_high, 72);
        assert_eq!(manifest.zones[1].playback_mode, PlaybackMode::OneShot);
    }

    #[test]
    fn unsupported_opcodes_are_all_reported_and_output_is_not_created() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("unsupported.sfz");
        let output = directory.path().join("unsupported.shrinst");
        fs::write(
            &source,
            "<region> sample=tone.wav cutoff=1200 resonance=4\n",
        )
        .unwrap();
        let error = import_sfz(&source, &output, metadata())
            .unwrap_err()
            .to_string();
        assert!(error.contains("cutoff"));
        assert!(error.contains("resonance"));
        assert!(!output.exists());
    }

    #[test]
    fn unrepresentable_loop_and_sample_escape_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("bad.sfz");
        let output = directory.path().join("bad.shrinst");
        write_synthetic_wav(&directory.path().join("tone.wav"));
        fs::write(
            &source,
            "<region> sample=tone.wav loop_mode=loop_continuous loop_start=2 loop_end=10\n",
        )
        .unwrap();
        assert!(
            import_sfz(&source, &output, metadata())
                .unwrap_err()
                .to_string()
                .contains("not representable")
        );

        fs::write(&source, "<region> sample=../outside.wav\n").unwrap();
        assert!(import_sfz(&source, &output, metadata()).is_err());
    }
}
