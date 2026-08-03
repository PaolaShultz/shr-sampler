use crate::schema::{
    Adsr, InstrumentManifest, LoopMode, MAX_DECODED_SAMPLE_BYTES, MAX_MANIFEST_BYTES,
    MAX_PACKAGE_DIRECTORIES, MAX_PACKAGE_FILES, MAX_SAMPLE_FILE_BYTES, PlaybackMode,
    SampleMetadata,
};
use hound::SampleFormat;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Take};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SampleFrame {
    pub left: f32,
    pub right: f32,
}

#[derive(Clone, Debug)]
pub struct PreparedSample {
    pub frames: Box<[SampleFrame]>,
    pub sample_rate: u32,
    pub original_channels: u16,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PreparedLoop {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedZone {
    pub sample_index: usize,
    pub key_low: u8,
    pub key_high: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
    pub root_note: u8,
    pub pitch_offset_cents: f32,
    pub left_gain: f32,
    pub right_gain: f32,
    pub playback_mode: PlaybackMode,
    pub loop_range: Option<PreparedLoop>,
    pub envelope: Adsr,
    pub sample_start: usize,
    pub sample_end: usize,
}

#[derive(Clone, Debug)]
pub struct PreparedInstrument {
    pub(crate) manifest: InstrumentManifest,
    pub(crate) samples: Box<[PreparedSample]>,
    pub(crate) zones: Box<[PreparedZone]>,
}

impl PreparedInstrument {
    pub fn manifest(&self) -> &InstrumentManifest {
        &self.manifest
    }

    pub fn zone_count(&self) -> usize {
        self.zones.len()
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    pub fn decoded_sample_bytes(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.frames.len() * size_of::<SampleFrame>())
            .sum()
    }
}

pub fn load_package(directory: impl AsRef<Path>) -> Result<PreparedInstrument, String> {
    let directory = directory.as_ref();
    if directory.extension().and_then(|value| value.to_str()) != Some("shrinst") {
        return Err(format!(
            "instrument package must end in .shrinst: {}",
            directory.display()
        ));
    }
    let root_metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("inspect package {}: {error}", directory.display()))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(format!(
            "instrument package must be a real directory, not a symlink: {}",
            directory.display()
        ));
    }
    let root = fs::canonicalize(directory)
        .map_err(|error| format!("canonicalize package {}: {error}", directory.display()))?;
    let actual_files = scan_package(&root)?;
    let manifest_path = root.join("manifest.json");
    let manifest_bytes = read_bounded_regular(&manifest_path, MAX_MANIFEST_BYTES, "manifest")?;
    let manifest =
        InstrumentManifest::from_json(&manifest_bytes).map_err(|error| error.to_string())?;

    let expected_files: BTreeSet<PathBuf> = std::iter::once(PathBuf::from("manifest.json"))
        .chain(
            manifest
                .zones
                .iter()
                .map(|zone| PathBuf::from(&zone.sample.path)),
        )
        .collect();
    if actual_files != expected_files {
        let unexpected = actual_files.difference(&expected_files).next();
        let missing = expected_files.difference(&actual_files).next();
        return Err(match (unexpected, missing) {
            (Some(path), _) => format!("unreferenced file in package: {}", path.display()),
            (_, Some(path)) => format!("referenced package file is missing: {}", path.display()),
            _ => "package file set does not match manifest".into(),
        });
    }

    let mut samples = Vec::new();
    let mut sample_indices = BTreeMap::new();
    for zone in &manifest.zones {
        if sample_indices.contains_key(&zone.sample.path) {
            continue;
        }
        let path = root.join(&zone.sample.path);
        verify_real_path_components(&root, &zone.sample.path)?;
        let bytes = read_bounded_regular(&path, MAX_SAMPLE_FILE_BYTES, "sample")?;
        let actual_hash = sha256_hex(&bytes);
        if !actual_hash.eq_ignore_ascii_case(&zone.sample.sha256) {
            return Err(format!(
                "{} hash mismatch: expected {}, found {}",
                zone.sample.path, zone.sample.sha256, actual_hash
            ));
        }
        let sample = decode_wav_bytes(&zone.sample.path, &bytes, Some(zone.sample.metadata))?;
        if sample.sample_rate != zone.sample.metadata.sample_rate
            || sample.original_channels != zone.sample.metadata.channels
            || sample.frames.len() as u64 != zone.sample.metadata.frames
        {
            return Err(format!(
                "{} decoded metadata does not match manifest",
                zone.sample.path
            ));
        }
        sample_indices.insert(zone.sample.path.clone(), samples.len());
        samples.push(sample);
    }

    let zones = manifest
        .zones
        .iter()
        .map(|zone| {
            let gain = db_to_gain(zone.gain_db);
            let (left_pan, right_pan) = balance_pan(zone.pan);
            let sample_start = usize::try_from(zone.sample_start_frame)
                .map_err(|_| format!("{} sample start does not fit this platform", zone.id))?;
            let sample_end =
                usize::try_from(zone.sample_end_frame.unwrap_or(zone.sample.metadata.frames))
                    .map_err(|_| format!("{} sample end does not fit this platform", zone.id))?;
            let loop_range = match zone.loop_mode {
                LoopMode::None => None,
                LoopMode::Forward {
                    start_frame,
                    end_frame,
                } => Some(PreparedLoop {
                    start: usize::try_from(start_frame).map_err(|_| {
                        format!("{} loop start does not fit this platform", zone.id)
                    })?,
                    end: usize::try_from(end_frame)
                        .map_err(|_| format!("{} loop end does not fit this platform", zone.id))?,
                }),
            };
            Ok(PreparedZone {
                sample_index: sample_indices[&zone.sample.path],
                key_low: zone.key_low,
                key_high: zone.key_high,
                velocity_low: zone.velocity_low,
                velocity_high: zone.velocity_high,
                root_note: zone.root_note,
                pitch_offset_cents: f32::from(zone.coarse_transpose) * 100.0 + zone.fine_tune_cents,
                left_gain: gain * left_pan,
                right_gain: gain * right_pan,
                playback_mode: zone.playback_mode,
                loop_range,
                envelope: zone.envelope,
                sample_start,
                sample_end,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    Ok(PreparedInstrument {
        manifest,
        samples: samples.into_boxed_slice(),
        zones: zones.into_boxed_slice(),
    })
}

fn scan_package(root: &Path) -> Result<BTreeSet<PathBuf>, String> {
    fn visit(
        root: &Path,
        directory: &Path,
        files: &mut BTreeSet<PathBuf>,
        directory_count: &mut usize,
    ) -> Result<(), String> {
        for entry in fs::read_dir(directory)
            .map_err(|error| format!("read package directory {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("read package entry: {error}"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("inspect package entry {}: {error}", path.display()))?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(format!(
                    "symlinks are not allowed in packages: {}",
                    path.display()
                ));
            }
            if file_type.is_dir() {
                *directory_count += 1;
                if *directory_count > MAX_PACKAGE_DIRECTORIES {
                    return Err(format!(
                        "package exceeds {MAX_PACKAGE_DIRECTORIES} directories"
                    ));
                }
                visit(root, &path, files, directory_count)?;
            } else if file_type.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| "package entry escaped its root".to_string())?
                    .to_path_buf();
                files.insert(relative);
                if files.len() > MAX_PACKAGE_FILES {
                    return Err(format!("package exceeds {MAX_PACKAGE_FILES} files"));
                }
            } else {
                return Err(format!(
                    "special files are not allowed in packages: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    let mut files = BTreeSet::new();
    let mut directory_count = 0;
    visit(root, root, &mut files, &mut directory_count)?;
    Ok(files)
}

fn verify_real_path_components(root: &Path, relative: &str) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for component in Path::new(relative).components() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("inspect {}: {error}", current.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "symlinks are not allowed in packages: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn read_bounded_regular(
    path: &Path,
    limit: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc_o_nofollow())
        .open(path)
        .map_err(|error| format!("open {label} {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {label} {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(format!(
            "{label} {} is not regular or exceeds {} bytes",
            path.display(),
            limit
        ));
    }
    read_limited(file.take(limit + 1), limit, label, path)
}

fn read_limited(
    mut reader: Take<File>,
    limit: u64,
    label: &str,
    path: &Path,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(usize::try_from(reader.limit().min(limit)).unwrap_or(0));
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {label} {}: {error}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{label} {} exceeds {limit} bytes", path.display()));
    }
    Ok(bytes)
}

const fn libc_o_nofollow() -> i32 {
    // Linux O_NOFOLLOW. This project currently targets Linux; checking every
    // component above supplies the readable error while this closes the final
    // component at the kernel boundary.
    0o400000
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub(crate) fn inspect_wav_bytes(label: &str, bytes: &[u8]) -> Result<SampleMetadata, String> {
    let sample = decode_wav_bytes(label, bytes, None)?;
    Ok(SampleMetadata {
        sample_rate: sample.sample_rate,
        channels: sample.original_channels,
        frames: sample.frames.len() as u64,
    })
}

fn decode_wav_bytes(
    label: &str,
    bytes: &[u8],
    expected: Option<SampleMetadata>,
) -> Result<PreparedSample, String> {
    let cursor = std::io::Cursor::new(bytes);
    let mut reader =
        hound::WavReader::new(cursor).map_err(|error| format!("decode {label}: {error}"))?;
    let specification = reader.spec();
    if !(1..=2).contains(&specification.channels) {
        return Err(format!("{label} must be mono or stereo"));
    }
    if !(8_000..=384_000).contains(&specification.sample_rate) {
        return Err(format!("{label} sample rate must be 8,000..=384,000 Hz"));
    }
    let declared_frames = u64::from(reader.duration());
    if declared_frames == 0 || declared_frames.saturating_mul(8) > MAX_DECODED_SAMPLE_BYTES {
        return Err(format!(
            "{label} exceeds the decoded stereo sample memory bound"
        ));
    }
    if expected.is_some_and(|metadata| {
        specification.sample_rate != metadata.sample_rate
            || specification.channels != metadata.channels
            || declared_frames != metadata.frames
    }) {
        return Err(format!("{label} decoded metadata does not match manifest"));
    }
    let channels = usize::from(specification.channels);
    let mut frames = Vec::with_capacity(declared_frames as usize);
    match (specification.sample_format, specification.bits_per_sample) {
        (SampleFormat::Float, 32) => {
            let mut samples = reader.samples::<f32>();
            while let Some(left) = samples.next() {
                let left = left.map_err(|error| format!("decode {label}: {error}"))?;
                let right = if channels == 2 {
                    samples
                        .next()
                        .ok_or_else(|| format!("{label} has an incomplete stereo frame"))?
                        .map_err(|error| format!("decode {label}: {error}"))?
                } else {
                    left
                };
                if !left.is_finite() || !right.is_finite() {
                    return Err(format!("{label} contains a non-finite float sample"));
                }
                frames.push(SampleFrame {
                    left: left.clamp(-1.0, 1.0),
                    right: right.clamp(-1.0, 1.0),
                });
            }
        }
        (SampleFormat::Int, bits @ 8..=32) => {
            let scale = (1_u64 << (bits - 1)) as f32;
            let mut samples = reader.samples::<i32>();
            while let Some(left) = samples.next() {
                let left = left.map_err(|error| format!("decode {label}: {error}"))? as f32 / scale;
                let right = if channels == 2 {
                    samples
                        .next()
                        .ok_or_else(|| format!("{label} has an incomplete stereo frame"))?
                        .map_err(|error| format!("decode {label}: {error}"))?
                        as f32
                        / scale
                } else {
                    left
                };
                frames.push(SampleFrame {
                    left: left.clamp(-1.0, 1.0),
                    right: right.clamp(-1.0, 1.0),
                });
            }
        }
        _ => return Err(format!("{label} has an unsupported WAV encoding")),
    }
    if frames.is_empty() {
        return Err(format!("{label} contains no complete audio frames"));
    }
    Ok(PreparedSample {
        frames: frames.into_boxed_slice(),
        sample_rate: specification.sample_rate,
        original_channels: specification.channels,
    })
}

fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn balance_pan(pan: f32) -> (f32, f32) {
    if pan < 0.0 {
        (1.0, 1.0 + pan)
    } else {
        (1.0 - pan, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{InstrumentMetadata, SampleRef, ZoneManifest};
    use std::os::unix::fs::symlink;

    fn wav_bytes(sample_rate: u32, channels: u16, frames: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut bytes);
            let mut writer = hound::WavWriter::new(
                cursor,
                hound::WavSpec {
                    channels,
                    sample_rate,
                    bits_per_sample: 16,
                    sample_format: SampleFormat::Int,
                },
            )
            .unwrap();
            for frame in 0..frames {
                for channel in 0..channels {
                    let value =
                        (((frame as i32 * 101) % 16_000) + i32::from(channel) * 1_000) as i16;
                    writer.write_sample(value).unwrap();
                }
            }
            writer.finalize().unwrap();
        }
        bytes
    }

    fn encoded_wav_bytes(bits: u16, float: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut bytes);
            let mut writer = hound::WavWriter::new(
                cursor,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 48_000,
                    bits_per_sample: bits,
                    sample_format: if float {
                        SampleFormat::Float
                    } else {
                        SampleFormat::Int
                    },
                },
            )
            .unwrap();
            if float {
                writer.write_sample(0.25_f32).unwrap();
                writer.write_sample(-0.5_f32).unwrap();
            } else {
                match bits {
                    8 => {
                        writer.write_sample(32_i8).unwrap();
                        writer.write_sample(-64_i8).unwrap();
                    }
                    16 => {
                        writer.write_sample(8_000_i16).unwrap();
                        writer.write_sample(-16_000_i16).unwrap();
                    }
                    24 | 32 => {
                        writer.write_sample(1_000_000_i32).unwrap();
                        writer.write_sample(-2_000_000_i32).unwrap();
                    }
                    _ => unreachable!(),
                }
            }
            writer.finalize().unwrap();
        }
        bytes
    }

    fn manifest(bytes: &[u8], channels: u16, frames: u64) -> InstrumentManifest {
        InstrumentManifest {
            format_version: 1,
            instrument_id: "fixture".into(),
            display_name: "Fixture".into(),
            metadata: InstrumentMetadata {
                author: "SHR tests".into(),
                source: "Synthetic".into(),
                licence: "MIT".into(),
                attribution: "None".into(),
                notes: "Generated".into(),
            },
            max_polyphony: 4,
            zones: vec![ZoneManifest {
                id: "zone-001".into(),
                sample: SampleRef {
                    path: "samples/tone.wav".into(),
                    sha256: sha256_hex(bytes),
                    metadata: SampleMetadata {
                        sample_rate: 48_000,
                        channels,
                        frames,
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

    fn write_package(root: &Path, manifest: &InstrumentManifest, bytes: &[u8]) {
        fs::create_dir_all(root.join("samples")).unwrap();
        fs::write(root.join("samples/tone.wav"), bytes).unwrap();
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn package_verifies_hash_metadata_and_mono_expansion() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("fixture.shrinst");
        let bytes = wav_bytes(48_000, 1, 32);
        write_package(&package, &manifest(&bytes, 1, 32), &bytes);
        let prepared = load_package(&package).unwrap();
        assert_eq!(prepared.sample_count(), 1);
        assert_eq!(prepared.decoded_sample_bytes(), 32 * 8);
        assert_eq!(
            prepared.samples[0].frames[7].left,
            prepared.samples[0].frames[7].right
        );

        let mut wrong_hash = manifest(&bytes, 1, 32);
        wrong_hash.zones[0].sample.sha256 = "f".repeat(64);
        fs::write(
            package.join("manifest.json"),
            serde_json::to_vec_pretty(&wrong_hash).unwrap(),
        )
        .unwrap();
        assert!(
            load_package(&package)
                .unwrap_err()
                .contains("hash mismatch")
        );
    }

    #[test]
    fn all_documented_integer_and_float_wav_encodings_decode() {
        for (bits, float) in [
            (8, false),
            (16, false),
            (24, false),
            (32, false),
            (32, true),
        ] {
            let bytes = encoded_wav_bytes(bits, float);
            let metadata = inspect_wav_bytes("synthetic.wav", &bytes).unwrap();
            assert_eq!(metadata.sample_rate, 48_000);
            assert_eq!(metadata.channels, 1);
            assert_eq!(metadata.frames, 2);
        }
    }

    #[test]
    fn non_finite_float_wav_samples_are_refused() {
        let mut bytes = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut bytes);
            let mut writer = hound::WavWriter::new(
                cursor,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 48_000,
                    bits_per_sample: 32,
                    sample_format: SampleFormat::Float,
                },
            )
            .unwrap();
            writer.write_sample(f32::NAN).unwrap();
            writer.finalize().unwrap();
        }
        assert!(
            inspect_wav_bytes("non-finite.wav", &bytes)
                .unwrap_err()
                .contains("non-finite")
        );
    }

    #[test]
    fn package_refuses_metadata_mismatch_and_malformed_audio() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("fixture.shrinst");
        let bytes = wav_bytes(48_000, 2, 16);
        write_package(&package, &manifest(&bytes, 1, 16), &bytes);
        assert!(load_package(&package).unwrap_err().contains("metadata"));

        write_package(&package, &manifest(&bytes, 2, 16), &bytes);
        let stereo = load_package(&package).unwrap();
        assert_ne!(
            stereo.samples[0].frames[0].left,
            stereo.samples[0].frames[0].right
        );

        let malformed = b"not a wav";
        write_package(&package, &manifest(malformed, 1, 16), malformed);
        assert!(load_package(&package).unwrap_err().contains("decode"));
    }

    #[test]
    fn package_refuses_an_escaping_sample_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("fixture.shrinst");
        let bytes = wav_bytes(48_000, 1, 16);
        write_package(&package, &manifest(&bytes, 1, 16), &bytes);
        fs::write(directory.path().join("outside.wav"), &bytes).unwrap();
        fs::remove_file(package.join("samples/tone.wav")).unwrap();
        symlink("../../outside.wav", package.join("samples/tone.wav")).unwrap();
        assert!(load_package(&package).unwrap_err().contains("symlinks"));
    }

    #[test]
    fn package_refuses_unreferenced_and_excessive_file_sets() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("fixture.shrinst");
        let bytes = wav_bytes(48_000, 1, 16);
        write_package(&package, &manifest(&bytes, 1, 16), &bytes);
        fs::write(package.join("unexpected.txt"), b"extra").unwrap();
        assert!(load_package(&package).unwrap_err().contains("unreferenced"));

        fs::remove_file(package.join("unexpected.txt")).unwrap();
        for index in 0..256 {
            fs::write(package.join(format!("samples/extra-{index:03}.wav")), b"").unwrap();
        }
        assert!(
            load_package(&package)
                .unwrap_err()
                .contains("exceeds 257 files")
        );
    }
}
