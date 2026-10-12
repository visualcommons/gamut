//! `gamut color` — inspect the shared color tables (gamut-color).

use clap::Subcommand;
use gamut::color::{
    BitDepth, ChromaSubsampling, ColorRange, ColourPrimaries, MatrixCoefficients,
    TransferCharacteristics,
};
use gamut::core::{Cmyk8, Gray8, Pixel, Rgb8, Rgba8};

use crate::error::CliError;

/// `gamut color` subcommands.
#[derive(Subcommand)]
pub(crate) enum ColorCommand {
    /// Print the CICP code points and the pixel-format / bit-depth / subsampling tables.
    List,
}

/// Runs a `color` subcommand.
pub(crate) fn run(cmd: &ColorCommand) -> Result<(), CliError> {
    match cmd {
        ColorCommand::List => {
            list();
            Ok(())
        }
    }
}

/// Prints a compile-time [`Pixel`] layout's constants.
fn print_pixel<P: Pixel>(name: &str) {
    println!(
        "  {name}: {} ch, {} bytes/pixel, {:?}",
        P::CHANNELS,
        P::BYTES_PER_PIXEL,
        P::MODEL,
    );
}

/// Prints the gamut-color enums alongside their spec code points / descriptors.
fn list() {
    println!("matrix coefficients (CICP code point):");
    for mc in [
        MatrixCoefficients::Identity,
        MatrixCoefficients::Bt709,
        MatrixCoefficients::Unspecified,
        MatrixCoefficients::Bt601,
        MatrixCoefficients::YCgCo,
        MatrixCoefficients::Bt2020Ncl,
    ] {
        println!("  {:>3}  {mc:?}", mc.code_point());
    }

    println!("colour primaries (CICP code point):");
    for cp in [
        ColourPrimaries::Bt709,
        ColourPrimaries::Unspecified,
        ColourPrimaries::Bt601Pal,
        ColourPrimaries::Smpte170m,
        ColourPrimaries::Bt2020,
        ColourPrimaries::DisplayP3,
    ] {
        println!("  {:>3}  {cp:?}", cp.code_point());
    }

    println!("transfer characteristics (CICP code point):");
    for tc in [
        TransferCharacteristics::Bt709,
        TransferCharacteristics::Unspecified,
        TransferCharacteristics::Bt601,
        TransferCharacteristics::Linear,
        TransferCharacteristics::Srgb,
        TransferCharacteristics::Bt2020_10,
        TransferCharacteristics::Bt2020_12,
        TransferCharacteristics::Pq,
        TransferCharacteristics::Hlg,
    ] {
        println!("  {:>3}  {tc:?}", tc.code_point());
    }

    println!("color range (flag):");
    for range in [ColorRange::Limited, ColorRange::Full] {
        println!("  {:>3}  {range:?}", range.flag());
    }

    println!("pixel layouts (channels, bytes/pixel, colour model):");
    print_pixel::<Gray8>("Gray8");
    print_pixel::<Rgb8>("Rgb8");
    print_pixel::<Rgba8>("Rgba8");
    print_pixel::<Cmyk8>("Cmyk8");

    println!("bit depths:");
    for bd in [
        BitDepth::Eight,
        BitDepth::Ten,
        BitDepth::Twelve,
        BitDepth::Sixteen,
    ] {
        println!("  {bd:?}: {} bits (max {})", bd.bits(), bd.max_value());
    }

    println!("chroma subsampling (subsampling_x, subsampling_y):");
    for cs in [
        ChromaSubsampling::Cs444,
        ChromaSubsampling::Cs422,
        ChromaSubsampling::Cs420,
        ChromaSubsampling::Cs400,
    ] {
        let (x, y) = cs.subsampling();
        println!("  {cs:?}: ({x}, {y})");
    }
}
