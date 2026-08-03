use hound::{SampleFormat, WavSpec, WavWriter};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn command(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shr-sampler"))
        .args(arguments)
        .output()
        .unwrap()
}

fn write_wav(path: &Path) {
    let mut writer = WavWriter::create(
        path,
        WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        },
    )
    .unwrap();
    for index in 0..800 {
        let phase = index % 80;
        let value = (phase as i16 - 40) * 500;
        writer.write_sample(value).unwrap();
    }
    writer.finalize().unwrap();
}

#[test]
fn import_validate_and_render_are_end_to_end_deterministic() {
    let directory = tempfile::tempdir().unwrap();
    let sfz = directory.path().join("instrument.sfz");
    let sample = directory.path().join("tone.wav");
    let package = directory.path().join("instrument.shrinst");
    let first = directory.path().join("first.wav");
    let second = directory.path().join("second.wav");
    write_wav(&sample);
    fs::write(
        &sfz,
        "<group> group_label=Synthetic\n<region> sample=tone.wav key=60 loop_mode=no_loop ampeg_attack=0.001 ampeg_release=0.01\n",
    )
    .unwrap();
    let import = command(&[
        "import-sfz",
        sfz.to_str().unwrap(),
        package.to_str().unwrap(),
        "--instrument-id",
        "cli-fixture",
        "--name",
        "CLI Fixture",
        "--author",
        "SHR tests",
        "--source",
        "Project-authored synthetic fixture",
        "--licence",
        "MIT",
    ]);
    assert!(
        import.status.success(),
        "{}",
        String::from_utf8_lossy(&import.stderr)
    );
    assert!(String::from_utf8_lossy(&import.stdout).contains("1 zones"));

    let validate = command(&["validate", package.to_str().unwrap()]);
    assert!(
        validate.status.success(),
        "{}",
        String::from_utf8_lossy(&validate.stderr)
    );
    assert!(String::from_utf8_lossy(&validate.stdout).contains("valid SHR instrument"));

    for output in [&first, &second] {
        let render = command(&[
            "render",
            package.to_str().unwrap(),
            output.to_str().unwrap(),
            "--sample-rate",
            "8000",
            "--seconds",
            "0.05",
            "--gate-seconds",
            "0.03",
            "--block-size",
            "17",
            "--velocity",
            "127",
        ]);
        assert!(
            render.status.success(),
            "{}",
            String::from_utf8_lossy(&render.stderr)
        );
    }
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    let reader = hound::WavReader::open(first).unwrap();
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_format, SampleFormat::Float);
    assert_eq!(reader.duration(), 400);
}

#[test]
fn import_reports_unsupported_sound_behavior() {
    let directory = tempfile::tempdir().unwrap();
    let sfz = directory.path().join("unsupported.sfz");
    let package = directory.path().join("unsupported.shrinst");
    fs::write(&sfz, "<region> sample=tone.wav cutoff=1200\n").unwrap();
    let output = command(&[
        "import-sfz",
        sfz.to_str().unwrap(),
        package.to_str().unwrap(),
        "--instrument-id",
        "unsupported",
        "--name",
        "Unsupported",
        "--author",
        "SHR tests",
        "--source",
        "Synthetic",
        "--licence",
        "MIT",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cutoff"));
    assert!(!package.exists());
}
