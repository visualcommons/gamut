//! `gamut av1` — AV1 still-image operations (gamut-av1).

use std::path::{Path, PathBuf};

use clap::Subcommand;
use gamut::av1::encode_still_lossless_identity;
use gamut::color::Planar8;

use crate::error::CliError;
use crate::input::decode_rgb8;

/// `gamut av1` subcommands.
#[derive(Subcommand)]
pub(crate) enum Av1Command {
    /// Encode an image to a raw AV1 OBU temporal unit (decode it with `dav1d -i out.obu`).
    Encode {
        /// Input image (PNG, JPEG, or PPM/P6).
        input: PathBuf,
        /// Output `.obu` file.
        output: PathBuf,
    },
}

/// Runs an `av1` subcommand.
pub(crate) fn run(cmd: &Av1Command) -> Result<(), CliError> {
    match cmd {
        Av1Command::Encode { input, output } => encode(input, output),
    }
}

/// Encodes `input` to a lossless AV1 temporal unit written to `output`.
fn encode(input: &Path, output: &Path) -> Result<(), CliError> {
    let (rgb, dims) = decode_rgb8(input)?;
    let planes = Planar8::from_rgb8_identity(&rgb, dims)?;
    let still = encode_still_lossless_identity(&planes)?;
    tracing::info!(
        seq_profile = still.config.seq_profile,
        seq_level_idx_0 = still.config.seq_level_idx_0,
        bytes = still.obus.len(),
        "encoded AV1 temporal unit"
    );

    std::fs::write(output, &still.obus).map_err(|source| CliError::Io {
        path: output.to_path_buf(),
        source,
    })?;

    println!(
        "wrote {} ({}x{}, {} bytes, lossless AV1 OBU)",
        output.display(),
        dims.width(),
        dims.height(),
        still.obus.len(),
    );
    Ok(())
}
