//! The CLI's error type, rendered to stderr by `main`.

use std::path::PathBuf;

/// Anything that can go wrong while running a `gamut` subcommand.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// A filesystem read or write failed.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// The file the operation targeted.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// An input image could not be decoded by the `image` crate.
    #[error("failed to decode image {path}: {source}")]
    Decode {
        /// The input file.
        path: PathBuf,
        /// The underlying decode error.
        #[source]
        source: image::ImageError,
    },

    /// A gamut codec rejected the input or hit an unsupported case.
    #[error(transparent)]
    Codec(#[from] gamut::core::Error),

    /// The `gamut-icc` parser rejected an embedded or standalone ICC profile.
    #[error(transparent)]
    Icc(#[from] gamut::icc::IccError),

    /// The requested output format is not (yet) supported by the CLI.
    #[error(
        "unsupported output format: {0} (supported: 'avif', 'webp', 'tiff', 'png', 'jxl', 'jpg'/'jpeg')"
    )]
    UnsupportedOutput(String),

    /// `gamut icc` found no embedded ICC profile in the input container.
    #[error("no embedded ICC profile found in {path}")]
    NoIccProfile {
        /// The input file.
        path: PathBuf,
    },

    /// `gamut inspect` found the file was not fully accounted for (the strict/archival check
    /// failed). The full report is printed to stdout first; this carries the summary rendered to
    /// stderr that drives the non-zero exit code.
    #[error("{0}")]
    NotFullyAccounted(String),

    /// `gamut inspect` sniffed an ISOBMFF file that is not the HEVC still image its HEIC arm
    /// reports on — an AVIF, say, which may carry the generic `mif1` brand as its major brand.
    /// Nothing is printed to stdout for it: this arm has no slice for that container.
    #[error(
        "{path}: unsupported container brand '{brand}' — gamut inspect reads HEVC still images (HEIF/HEIC) here"
    )]
    UnsupportedContainer {
        /// The input file.
        path: PathBuf,
        /// The file's `ftyp` major brand, with non-printable bytes escaped.
        brand: String,
    },

    /// A command argument was malformed in a way clap could not catch.
    #[error("invalid argument: {0}")]
    Usage(String),
}
