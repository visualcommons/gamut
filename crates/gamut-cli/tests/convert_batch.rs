//! End-to-end tests for batch `gamut convert` (issue #665): several inputs into an output
//! directory, `-` for standard input and output, the skip-existing default against
//! `-f`/`--overwrite`, `-q`, the exit code when one input of several fails, and the AVIF size
//! breakdown on the summary line.
//!
//! These drive the built `gamut` binary for the reason `convert_metadata.rs` gives: `gamut-cli` is
//! outside the mutation globs and the coverage regex, so only an end-to-end run pins the wiring.
//! How the paths are planned is pinned beside `plan` in `src/commands/convert.rs`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use gamut::avif::AvifContainer;
use gamut::core::{DecodeImage, Dimensions, EncodeImage, ImageBuf, ImageRef, Rgb8};
use gamut::png::{PngDecoder, PngEncoder};

/// The 2×2 RGB image every input carries.
const PIXELS: [u8; 12] = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];

/// A 2×2 RGB PNG of [`PIXELS`].
fn png() -> Vec<u8> {
    let dims = Dimensions {
        width: 2,
        height: 2,
    };
    PngEncoder::new()
        .encode_to_vec(ImageRef::<Rgb8>::new(&PIXELS, dims).unwrap())
        .unwrap()
}

/// A fresh, empty scratch directory for test `name`, removed by the returned guard.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("gamut-convert-batch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// `file` inside the scratch directory.
    fn path(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `gamut convert <args>` with `stdin` on standard input.
fn convert(args: &[&Path], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gamut"))
        .arg("convert")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run gamut convert");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

/// Lossy views of a run's two output streams, for assertion messages.
fn streams(run: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    )
}

#[test]
fn several_inputs_are_written_into_the_output_directory() {
    let scratch = Scratch::new("dir");
    std::fs::write(scratch.path("a.png"), png()).unwrap();
    std::fs::write(scratch.path("b.png"), png()).unwrap();
    let out = scratch.path("out");

    let run = convert(
        &[
            &scratch.path("a.png"),
            &scratch.path("b.png"),
            Path::new("-o"),
            &out,
        ],
        b"",
    );

    assert!(run.status.success(), "{}", streams(&run));
    for name in ["a.avif", "b.avif"] {
        let avif = std::fs::read(out.join(name)).unwrap();
        assert!(AvifContainer::parse(&avif).is_ok(), "{name} is not an AVIF");
    }
}

#[test]
fn standard_input_converts_to_standard_output() {
    let run = convert(
        &[
            Path::new("-"),
            Path::new("-o"),
            Path::new("-"),
            Path::new("--format"),
            Path::new("png"),
        ],
        &png(),
    );

    assert!(run.status.success(), "{}", streams(&run));
    let decoded: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&run.stdout).unwrap();
    assert_eq!(decoded.into_samples(), PIXELS);
}

#[test]
fn the_summary_goes_to_stderr_when_stdout_carries_the_image() {
    // The PNG decoder ignores bytes after IEND, so decoding stdout alone cannot see a summary
    // line appended to the image: stdout must end at IEND's CRC, and the line must be on stderr.
    let run = convert(
        &[
            Path::new("-"),
            Path::new("-o"),
            Path::new("-"),
            Path::new("--format"),
            Path::new("png"),
        ],
        &png(),
    );

    assert!(run.status.success(), "{}", streams(&run));
    let iend = run
        .stdout
        .windows(4)
        .rposition(|w| w == b"IEND")
        .unwrap_or_else(|| panic!("no IEND on stdout; {}", streams(&run)));
    assert_eq!(run.stdout.len(), iend + 8, "{}", streams(&run));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("wrote <stdout>"), "{}", streams(&run));
}

#[test]
fn a_lone_input_goes_into_an_existing_directory() {
    let scratch = Scratch::new("lone-dir");
    std::fs::write(scratch.path("in.png"), png()).unwrap();
    let out = scratch.path("out");
    std::fs::create_dir(&out).unwrap();

    let run = convert(&[&scratch.path("in.png"), &out], b"");

    assert!(run.status.success(), "{}", streams(&run));
    let avif = std::fs::read(out.join("in.avif")).unwrap();
    assert!(
        AvifContainer::parse(&avif).is_ok(),
        "in.avif is not an AVIF"
    );
}

#[test]
fn an_existing_output_is_kept_and_fails_the_run_without_overwrite() {
    let scratch = Scratch::new("keep");
    std::fs::write(scratch.path("in.png"), png()).unwrap();
    std::fs::write(scratch.path("out.png"), b"keep me").unwrap();

    let run = convert(&[&scratch.path("in.png"), &scratch.path("out.png")], b"");

    assert_eq!(run.status.code(), Some(1), "{}", streams(&run));
    assert_eq!(std::fs::read(scratch.path("out.png")).unwrap(), b"keep me");
}

#[test]
fn overwrite_replaces_an_existing_output() {
    let scratch = Scratch::new("overwrite");
    std::fs::write(scratch.path("in.png"), png()).unwrap();
    std::fs::write(scratch.path("out.png"), b"replace me").unwrap();

    let run = convert(
        &[
            &scratch.path("in.png"),
            &scratch.path("out.png"),
            Path::new("--overwrite"),
        ],
        b"",
    );

    assert!(run.status.success(), "{}", streams(&run));
    let written = std::fs::read(scratch.path("out.png")).unwrap();
    let decoded: ImageBuf<Rgb8> = PngDecoder::new().decode_image(&written).unwrap();
    assert_eq!(decoded.into_samples(), PIXELS);
}

#[test]
fn one_failing_input_exits_1_and_the_others_are_still_written() {
    let scratch = Scratch::new("fail");
    std::fs::write(scratch.path("good.png"), png()).unwrap();
    std::fs::write(scratch.path("bad.png"), b"not an image").unwrap();
    let out = scratch.path("out");

    let run = convert(
        &[
            &scratch.path("good.png"),
            &scratch.path("bad.png"),
            Path::new("-o"),
            &out,
            Path::new("--format"),
            Path::new("png"),
        ],
        b"",
    );

    assert_eq!(run.status.code(), Some(1), "{}", streams(&run));
    assert!(out.join("good.png").is_file(), "{}", streams(&run));
    assert!(!out.join("bad.png").exists());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("bad.png"), "{}", streams(&run));
    assert!(
        stderr.contains("1 of 2 conversions failed"),
        "{}",
        streams(&run)
    );
}

#[test]
fn quiet_prints_no_summary() {
    let scratch = Scratch::new("quiet");
    std::fs::write(scratch.path("in.png"), png()).unwrap();

    let run = convert(
        &[
            &scratch.path("in.png"),
            &scratch.path("out.png"),
            Path::new("-q"),
        ],
        b"",
    );

    assert!(run.status.success(), "{}", streams(&run));
    assert!(run.stdout.is_empty(), "{}", streams(&run));
}

#[test]
fn the_avif_summary_breaks_the_size_down() {
    let scratch = Scratch::new("breakdown");
    std::fs::write(scratch.path("in.png"), png()).unwrap();

    let run = convert(&[&scratch.path("in.png"), &scratch.path("out.avif")], b"");

    assert!(run.status.success(), "{}", streams(&run));
    let total = std::fs::read(scratch.path("out.avif")).unwrap().len();
    let stdout = String::from_utf8_lossy(&run.stdout);
    // `colour C + alpha A + container K bytes`, which must add up to the file. An RGB input has
    // no alpha auxiliary, so A is 0.
    let parts: Vec<usize> = stdout
        .split("; colour ")
        .nth(1)
        .and_then(|tail| tail.split(" bytes").next())
        .map(|sum| {
            sum.split(" + ")
                .map(|part| part.rsplit(' ').next().unwrap().parse().unwrap())
                .collect()
        })
        .unwrap_or_else(|| panic!("no breakdown in {stdout}"));
    assert_eq!(parts.len(), 3, "{stdout}");
    assert!(parts[0] > 0, "{stdout}");
    assert_eq!(parts[1], 0, "{stdout}");
    assert_eq!(parts.iter().sum::<usize>(), total, "{stdout}");
}
