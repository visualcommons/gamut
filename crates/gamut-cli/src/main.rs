//! `gamut` — a command-line sandbox over the gamut image codecs and their shared primitives.
//!
//! It decodes input — PNG/JPEG/PPM via the third-party [`image`] crate, and WebP via gamut's own
//! decoder — and encodes exclusively with the gamut crates, and surfaces the shared `color` /
//! `dsp` / `bitstream` primitives as inspection subcommands so the latest workspace features are
//! exercisable without writing throwaway Rust. See the crate README for the full command reference.
#![forbid(unsafe_code)]

mod commands;
mod error;
mod input;

use std::process::ExitCode;
use std::sync::LazyLock;

use clap::{Parser, Subcommand};

/// Detailed version string for `-V`/`--version`: the CLI's package version, the resolved
/// `gamut` library version (read from its `CARGO_PKG_VERSION` via [`gamut::VERSION`], not
/// hardcoded), plus build provenance (git commit, working-tree state, build profile, target
/// triple, rustc, commit date, and build timestamp) captured at compile time by `build.rs`.
/// Useful for pinning down exactly which build a bug report came from.
///
/// Built lazily once because the library version is a `const` rather than a string literal, so
/// the whole string can't be assembled by `concat!`.
static LONG_VERSION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{cli}\n\
         gamut library: {lib}\n\
         commit:        {hash} ({dirty})\n\
         profile:       {profile}\n\
         target:        {target}\n\
         rustc:         {rustc}\n\
         commit date:   {commit_date}\n\
         built:         {built}",
        cli = env!("CARGO_PKG_VERSION"),
        lib = gamut::VERSION,
        hash = env!("GAMUT_GIT_HASH"),
        dirty = env!("GAMUT_GIT_DIRTY"),
        profile = env!("GAMUT_BUILD_PROFILE"),
        target = env!("GAMUT_BUILD_TARGET"),
        rustc = env!("GAMUT_RUSTC_VERSION"),
        commit_date = env!("GAMUT_COMMIT_DATE"),
        built = env!("GAMUT_BUILD_TIMESTAMP"),
    )
});

/// Sandbox CLI for the gamut codecs and primitives.
#[derive(Parser)]
#[command(name = "gamut", version = LONG_VERSION.as_str(), about)]
struct Cli {
    /// Increase log verbosity (`-v` = info, `-vv` = debug). `RUST_LOG` overrides this.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

/// Top-level subcommands, each grouped by the crate it exercises.
#[derive(Subcommand)]
enum Command {
    /// Decode an image (PNG/JPEG/PPM/WebP/JXL) and re-encode it as AVIF/WebP/TIFF/PNG/JXL/JPEG.
    Convert(commands::convert::ConvertArgs),
    /// Strictly deconstruct a TIFF, DNG or PNG: account every byte, flag unknowns, and for PNG report where the bytes went (gamut-tiff/gamut-dng/gamut-png).
    Inspect(commands::inspect::InspectArgs),
    /// Extract and inspect the embedded ICC colour profile of an image (gamut-icc).
    Icc(commands::icc::IccArgs),
    /// Read/write ISOBMFF still-image containers: inspect, remux, build (gamut-isobmff).
    #[command(subcommand)]
    Isobmff(commands::isobmff::IsobmffCommand),
    /// AV1 still-image operations (gamut-av1).
    #[command(subcommand)]
    Av1(commands::av1::Av1Command),
    /// Inspect color tables: CICP code points and pixel formats (gamut-color).
    #[command(subcommand)]
    Color(commands::color::ColorCommand),
    /// Run the Walsh–Hadamard transform (gamut-dsp).
    #[command(subcommand)]
    Dsp(commands::dsp::DspCommand),
    /// Exercise bitstream primitives (gamut-bitstream).
    #[command(subcommand)]
    Bitstream(commands::bitstream::BitstreamCommand),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Convert(args) => commands::convert::run(&args),
        Command::Inspect(args) => commands::inspect::run(&args),
        Command::Icc(args) => commands::icc::run(&args),
        Command::Isobmff(cmd) => commands::isobmff::run(&cmd),
        Command::Av1(cmd) => commands::av1::run(&cmd),
        Command::Color(cmd) => commands::color::run(&cmd),
        Command::Dsp(cmd) => commands::dsp::run(&cmd),
        Command::Bitstream(cmd) => commands::bitstream::run(&cmd),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Initializes a stderr `tracing` subscriber. `RUST_LOG` takes precedence; otherwise the
/// `-v`/`--verbose` count maps to a level (0 = warn, 1 = info, 2+ = debug). Logs go to stderr so
/// stdout stays clean for command output.
fn init_tracing(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    let filter = match std::env::var("RUST_LOG") {
        Ok(directives) if !directives.is_empty() => EnvFilter::new(directives),
        _ => EnvFilter::new(match verbose {
            0 => "warn",
            1 => "info",
            _ => "debug",
        }),
    };

    // `try_init` fails only if a subscriber is already set; nothing to recover, so ignore it.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
