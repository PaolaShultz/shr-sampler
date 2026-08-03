pub mod host;

use shr_sampler_core::{
    Event, ImportMetadata, PreparedInstrument, RenderEvent, RenderSpec, import_sfz, load_package,
    write_wav,
};
use std::path::Path;

pub const USAGE: &str = r#"Usage:
  shr-sampler --version
  shr-sampler --client-name NAME --instrument /path/to/file.shrinst
  shr-sampler validate <instrument.shrinst>
  shr-sampler import-sfz <input.sfz> <output.shrinst> --instrument-id ID --name NAME --author TEXT --source TEXT --licence TEXT [--attribution TEXT] [--notes TEXT] [--max-polyphony 1..64]
  shr-sampler render <instrument.shrinst> <output.wav> [--note 0..127] [--velocity 1..127] [--seconds 0..600] [--gate-seconds 0..600] [--sample-rate 8000..384000] [--block-size 1..65536]

The live host attaches only to an existing JACK server, exposes out_l/out_r and
one ALSA Sequencer input port, and never creates connections. The importer
accepts only the documented controlled SFZ profile and never invokes
ConvertWithMoss."#;

pub fn run(args: Vec<String>) -> Result<String, String> {
    run_with_live_host(args, host::run)
}

pub fn run_with_live_host<F>(args: Vec<String>, live_host: F) -> Result<String, String>
where
    F: FnOnce(&str, &PreparedInstrument) -> Result<(), String>,
{
    match args.first().map(String::as_str) {
        Some("--help" | "-h") if args.len() == 1 => Ok(USAGE.into()),
        Some("--version" | "-V") if args.len() == 1 => {
            Ok(format!("shr-sampler {}", env!("CARGO_PKG_VERSION")))
        }
        Some("--client-name") => live_command(&args, live_host),
        Some("validate") if args.len() == 2 => validate(&args[1]),
        Some("import-sfz") if args.len() >= 3 => import(&args[1..]),
        Some("render") if args.len() >= 3 => render_command(&args[1..]),
        Some(command) => Err(format!("unknown or incomplete command `{command}`")),
        None => Err("missing command".into()),
    }
}

fn live_command<F>(args: &[String], live_host: F) -> Result<String, String>
where
    F: FnOnce(&str, &PreparedInstrument) -> Result<(), String>,
{
    if args.len() != 4 || args[0] != "--client-name" || args[2] != "--instrument" {
        return Err("live host requires --client-name NAME --instrument FILE".into());
    }
    host::validate_client_name(&args[1])?;
    // Strict package validation, hashing, WAV decoding, and all allocation
    // complete before the launcher may open JACK or ALSA.
    let instrument = load_package(&args[3])?;
    live_host(&args[1], &instrument)?;
    Ok("SHR Sampler stopped cleanly".into())
}

fn validate(path: &str) -> Result<String, String> {
    let instrument = load_package(path)?;
    Ok(format!(
        "valid SHR instrument: {} ({} zones, {} samples, {} decoded bytes)",
        instrument.manifest().display_name,
        instrument.zone_count(),
        instrument.sample_count(),
        instrument.decoded_sample_bytes()
    ))
}

fn import(args: &[String]) -> Result<String, String> {
    let options = parse_options(&args[2..])?;
    let required = |name: &str| {
        options
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| format!("missing required option --{name}"))
    };
    let max_polyphony = option(&options, "max-polyphony").map_or(Ok(16usize), |value| {
        value
            .parse::<usize>()
            .ok()
            .filter(|value| (1..=64).contains(value))
            .ok_or_else(|| "--max-polyphony must be 1..=64".to_string())
    })?;
    reject_unknown_options(
        &options,
        &[
            "instrument-id",
            "name",
            "author",
            "source",
            "licence",
            "attribution",
            "notes",
            "max-polyphony",
        ],
    )?;
    let report = import_sfz(
        &args[0],
        &args[1],
        ImportMetadata {
            instrument_id: required("instrument-id")?,
            display_name: required("name")?,
            author: required("author")?,
            source: required("source")?,
            licence: required("licence")?,
            attribution: option(&options, "attribution")
                .cloned()
                .unwrap_or_else(|| "None supplied".into()),
            notes: option(&options, "notes")
                .cloned()
                .unwrap_or_else(|| "Imported through the SHR controlled SFZ profile".into()),
            max_polyphony,
        },
    )
    .map_err(|error| error.to_string())?;
    let ignored = if report.ignored_harmless_opcodes.is_empty() {
        "none".into()
    } else {
        report.ignored_harmless_opcodes.join(", ")
    };
    Ok(format!(
        "imported {} ({} zones, {} samples; harmless ignored: {ignored})",
        args[1], report.zones, report.sample_files
    ))
}

fn render_command(args: &[String]) -> Result<String, String> {
    let options = parse_options(&args[2..])?;
    reject_unknown_options(
        &options,
        &[
            "note",
            "velocity",
            "seconds",
            "gate-seconds",
            "sample-rate",
            "block-size",
        ],
    )?;
    let note = parse_bounded::<u8>(&options, "note", 60, 0, 127)?;
    let velocity = parse_bounded::<u8>(&options, "velocity", 100, 1, 127)?;
    let sample_rate = parse_bounded::<u32>(&options, "sample-rate", 48_000, 8_000, 384_000)?;
    let block_size = parse_bounded::<usize>(&options, "block-size", 128, 1, 65_536)?;
    let seconds = parse_finite(&options, "seconds", 2.0, 0.0, 600.0, false)?;
    let gate_seconds = parse_finite(
        &options,
        "gate-seconds",
        (seconds * 0.75).max(f32::EPSILON),
        0.0,
        seconds,
        true,
    )?;
    let frames = (f64::from(seconds) * f64::from(sample_rate)).round() as u64;
    if frames == 0 {
        return Err("render duration rounds to zero frames".into());
    }
    let gate_frame = (f64::from(gate_seconds) * f64::from(sample_rate)).round() as u64;
    let mut events = vec![RenderEvent {
        frame: 0,
        event: Event::NoteOn { note, velocity },
    }];
    if gate_frame < frames {
        events.push(RenderEvent {
            frame: gate_frame,
            event: Event::NoteOff { note },
        });
    }
    let output = Path::new(&args[1]);
    if output.exists() {
        return Err(format!(
            "refusing to replace existing render {}",
            output.display()
        ));
    }
    let instrument = load_package(&args[0])?;
    write_wav(
        output,
        &instrument,
        RenderSpec {
            sample_rate,
            frames,
            block_size,
        },
        &events,
    )
    .map_err(|error| error.to_string())?;
    Ok(format!(
        "rendered {} deterministic stereo frames to {}",
        frames, args[1]
    ))
}

fn parse_options(args: &[String]) -> Result<Vec<(String, String)>, String> {
    if !args.len().is_multiple_of(2) {
        return Err(format!("missing value for {}", args.last().unwrap()));
    }
    let mut options = Vec::with_capacity(args.len() / 2);
    for pair in args.chunks_exact(2) {
        let name = pair[0]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected option, found `{}`", pair[0]))?;
        if options.iter().any(|(existing, _)| existing == name) {
            return Err(format!("duplicate option --{name}"));
        }
        options.push((name.into(), pair[1].clone()));
    }
    Ok(options)
}

fn option<'a>(options: &'a [(String, String)], name: &str) -> Option<&'a String> {
    options
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

fn reject_unknown_options(options: &[(String, String)], accepted: &[&str]) -> Result<(), String> {
    if let Some((name, _)) = options
        .iter()
        .find(|(name, _)| !accepted.contains(&name.as_str()))
    {
        return Err(format!("unknown option --{name}"));
    }
    Ok(())
}

fn parse_bounded<T>(
    options: &[(String, String)],
    name: &str,
    default: T,
    minimum: T,
    maximum: T,
) -> Result<T, String>
where
    T: Copy + Ord + std::str::FromStr + std::fmt::Display,
{
    let Some(source) = option(options, name) else {
        return Ok(default);
    };
    let value = source
        .parse::<T>()
        .map_err(|_| format!("--{name} must be an integer"))?;
    if !(minimum..=maximum).contains(&value) {
        return Err(format!("--{name} must be {minimum}..={maximum}"));
    }
    Ok(value)
}

fn parse_finite(
    options: &[(String, String)],
    name: &str,
    default: f32,
    minimum: f32,
    maximum: f32,
    include_zero: bool,
) -> Result<f32, String> {
    let Some(source) = option(options, name) else {
        return Ok(default);
    };
    let value = source
        .parse::<f32>()
        .map_err(|_| format!("--{name} must be a number"))?;
    let lower_ok = if include_zero {
        value >= minimum
    } else {
        value > minimum
    };
    if !value.is_finite() || !lower_ok || value > maximum {
        return Err(format!("--{name} is outside its finite supported range"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn version_is_machine_readable_and_matches_the_package() {
        assert_eq!(
            run(vec!["--version".into()]).unwrap(),
            format!("shr-sampler {}", env!("CARGO_PKG_VERSION"))
        );
        assert!(run(vec!["--version".into(), "extra".into()]).is_err());
    }

    #[test]
    fn live_arguments_are_strictly_validated() {
        for args in [
            vec!["--client-name", "name"],
            vec!["--client-name", "", "--instrument", "file.shrinst"],
            vec!["--instrument", "file.shrinst", "--client-name", "name"],
            vec![
                "--client-name",
                "name",
                "--instrument",
                "file.shrinst",
                "extra",
            ],
        ] {
            let strings = args.into_iter().map(str::to_string).collect();
            assert!(run_with_live_host(strings, |_, _| Ok(())).is_err());
        }
    }

    #[test]
    fn package_failure_is_rejected_before_live_host_activation() {
        let called = Cell::new(false);
        let args = vec![
            "--client-name".into(),
            "shr-sampler-test".into(),
            "--instrument".into(),
            "/does/not/exist.shrinst".into(),
        ];
        let error = run_with_live_host(args, |_, _| {
            called.set(true);
            Ok(())
        })
        .unwrap_err();
        assert!(!called.get());
        assert!(error.contains("inspect package"));
    }
}
