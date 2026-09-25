//! `gamut isobmff` — read and write ISOBMFF/HEIF still-image containers (gamut-isobmff).
//!
//! The container layer is codec-agnostic: the coded bitstream is carried as opaque bytes, so these
//! subcommands exercise the whole `gamut-isobmff` read/write surface without a working AV1/HEVC
//! codec. `inspect` parses a real `.avif`/`.heic` and prints its box structure; `remux` re-serialises
//! a container (proving the read→write round-trip on real files); `build` constructs a synthetic
//! container that exercises every modelled box, property, reference, and group.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use gamut::isobmff::{
    self, ColourInformation, EntityGroup, ImageGrid, IsoBmffImage, Item, ItemReference, NclxColr,
    Property, PropertyKind,
};

use crate::error::CliError;

/// `gamut isobmff` subcommands.
#[derive(Subcommand)]
pub(crate) enum IsobmffCommand {
    /// Parse a still-image container (.avif/.heic) and print its box structure.
    Inspect {
        /// Input `.avif` or `.heic` file.
        input: PathBuf,
    },
    /// Re-mux a container: parse it and write a normalised copy (coded payloads preserved verbatim).
    Remux {
        /// Input `.avif` or `.heic` file.
        input: PathBuf,
        /// Output container file.
        output: PathBuf,
    },
    /// Build a synthetic container exercising the full model, with placeholder coded payloads.
    Build {
        /// Output container file.
        output: PathBuf,
    },
}

/// Runs an `isobmff` subcommand.
pub(crate) fn run(cmd: &IsobmffCommand) -> Result<(), CliError> {
    match cmd {
        IsobmffCommand::Inspect { input } => inspect(input),
        IsobmffCommand::Remux { input, output } => remux(input, output),
        IsobmffCommand::Build { output } => build(output),
    }
}

/// Parses `input` and prints its container structure.
fn inspect(input: &Path) -> Result<(), CliError> {
    let data = read_file(input)?;
    let img = isobmff::read(&data)?;
    print_container(input, data.len(), &img);
    Ok(())
}

/// Parses `input`, re-serialises it to `output`, and verifies the read→write round-trip.
fn remux(input: &Path, output: &Path) -> Result<(), CliError> {
    let data = read_file(input)?;
    let model = isobmff::read(&data)?;
    let bytes = isobmff::write(&model)?;
    // Round-trip guard: re-parsing our own output must reproduce the model exactly.
    if isobmff::read(&bytes)? != model {
        return Err(CliError::Codec(gamut::core::Error::InvalidInput(
            "isobmff remux: re-parsed container did not match the source model",
        )));
    }
    write_file(output, &bytes)?;
    println!(
        "read {} ({} bytes): {} item(s), primary item {}",
        input.display(),
        data.len(),
        model.items.len(),
        model.primary_item_id,
    );
    println!(
        "wrote {} ({} bytes) — normalised container (single-extent mdat, smallest box versions); \
         coded payloads preserved verbatim",
        output.display(),
        bytes.len(),
    );
    println!("round-trip verified: read(write(read(input))) == read(input)");
    Ok(())
}

/// Builds the demonstration container and writes it to `output`.
fn build(output: &Path) -> Result<(), CliError> {
    let img = demo_image()?;
    let bytes = isobmff::write(&img)?;
    write_file(output, &bytes)?;
    println!(
        "wrote {} ({} bytes): {} item(s) exercising the full still-image model, primary item {}",
        output.display(),
        bytes.len(),
        img.items.len(),
        img.primary_item_id,
    );
    println!(
        "note: coded payloads are placeholders — the container is structurally valid but pixels \
         will not decode",
    );
    Ok(())
}

/// Reads a file into memory, attaching the path to any I/O error.
fn read_file(path: &Path) -> Result<Vec<u8>, CliError> {
    std::fs::read(path).map_err(|source| CliError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes `bytes` to `path`, attaching the path to any I/O error.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    std::fs::write(path, bytes).map_err(|source| CliError::Io {
        path: path.to_path_buf(),
        source,
    })
}

// ---- structural printing -------------------------------------------------------------------

/// Prints the full container structure: brands, primary item, each item's properties/references,
/// and the entity groups.
fn print_container(path: &Path, byte_len: usize, img: &IsoBmffImage) {
    println!("container: {} ({} bytes)", path.display(), byte_len);
    println!(
        "  ftyp: {} (minor {}) compatible=[{}]",
        fourcc(&img.major_brand),
        img.minor_version,
        img.compatible_brands
            .iter()
            .map(fourcc)
            .collect::<Vec<_>>()
            .join(", "),
    );
    println!("  primary: item {}", img.primary_item_id);
    println!("  items ({}):", img.items.len());
    for item in &img.items {
        print_item(img, item);
    }
    if img.groups.is_empty() {
        println!("  groups: none");
    } else {
        println!("  groups ({}):", img.groups.len());
        for g in &img.groups {
            println!(
                "    {} #{} -> {:?}",
                fourcc(&g.group_type),
                g.group_id,
                g.entity_ids,
            );
        }
    }
}

/// Prints one item and its properties, grid geometry (if a `grid`), references, and payload size.
fn print_item(img: &IsoBmffImage, item: &Item) {
    let mut tags = Vec::new();
    if item.id == img.primary_item_id {
        tags.push("primary".to_string());
    }
    if item.hidden {
        tags.push("hidden".to_string());
    }
    if !item.name.is_empty() {
        tags.push(format!("name=\"{}\"", item.name));
    }
    if let Some(ct) = &item.content_type {
        let enc = item
            .content_encoding
            .as_deref()
            .map(|e| format!(";{e}"))
            .unwrap_or_default();
        tags.push(format!("mime={ct}{enc}"));
    }
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", tags.join(", "))
    };
    println!(
        "    item {}  {}{}",
        item.id,
        fourcc(&item.item_type),
        suffix
    );

    if item.properties.is_empty() {
        println!("      properties: none");
    } else {
        println!("      properties ({}):", item.properties.len());
        for p in &item.properties {
            println!("        {}", render_property(p));
        }
    }

    if item.item_type == *b"grid" {
        match ImageGrid::parse(&item.payload) {
            Ok(g) => println!(
                "      grid: {}x{} tiles -> {}x{}",
                g.rows, g.columns, g.output_width, g.output_height,
            ),
            Err(e) => println!("      grid: <unparsable payload: {e}>"),
        }
    }

    if item.references.is_empty() {
        println!("      references: none");
    } else {
        println!("      references:");
        for r in &item.references {
            println!(
                "        {} -> {:?}",
                fourcc(&r.reference_type),
                r.to_item_ids
            );
        }
    }
    println!("      payload: {} bytes", item.payload.len());
}

/// Renders one property as a single human-readable line.
fn render_property(p: &Property) -> String {
    let body = match &p.kind {
        PropertyKind::ImageSpatialExtents { width, height } => format!("ispe  {width}x{height}"),
        PropertyKind::PixelInformation { bits_per_channel } => format!(
            "pixi  {} bpc",
            bits_per_channel
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join("/"),
        ),
        PropertyKind::Colour(ColourInformation::Nclx(n)) => format!(
            "colr  nclx cp={} tc={} mc={} {}",
            n.colour_primaries,
            n.transfer_characteristics,
            n.matrix_coefficients,
            if n.full_range {
                "full-range"
            } else {
                "limited-range"
            },
        ),
        PropertyKind::Colour(ColourInformation::RestrictedIcc(d)) => {
            format!("colr  rICC ({} bytes)", d.len())
        }
        PropertyKind::Colour(ColourInformation::UnrestrictedIcc(d)) => {
            format!("colr  prof ({} bytes)", d.len())
        }
        PropertyKind::Rotation(n) => format!("irot  {n} (x90 ccw)"),
        PropertyKind::Mirror(axis) => format!(
            "imir  axis={axis} ({})",
            if *axis == 0 { "vertical" } else { "horizontal" },
        ),
        PropertyKind::CleanAperture {
            width_n,
            width_d,
            height_n,
            height_d,
            ..
        } => format!("clap  {width_n}/{width_d} x {height_n}/{height_d}"),
        PropertyKind::PixelAspectRatio {
            h_spacing,
            v_spacing,
        } => format!("pasp  {h_spacing}:{v_spacing}"),
        PropertyKind::AuxiliaryType {
            aux_type,
            aux_subtype,
        } => {
            let sub = if aux_subtype.is_empty() {
                String::new()
            } else {
                format!(" (+{} subtype bytes)", aux_subtype.len())
            };
            format!("auxC  {aux_type}{sub}")
        }
        PropertyKind::ContentLightLevel {
            max_content_light_level,
            max_pic_average_light_level,
        } => {
            format!("clli  MaxCLL={max_content_light_level} MaxPALL={max_pic_average_light_level}")
        }
        PropertyKind::CodecConfiguration { kind, data } => {
            format!("{}  codec config ({} bytes)", fourcc(kind), data.len())
        }
        PropertyKind::Other { kind, data } => {
            format!("{}  ({} bytes, carried verbatim)", fourcc(kind), data.len())
        }
        _ => "<unrecognised property>".to_string(),
    };
    if p.essential {
        format!("{body}  [essential]")
    } else {
        body
    }
}

/// Renders a four-character code as its ASCII text, or hex if it is not printable.
fn fourcc(code: &[u8; 4]) -> String {
    if code.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        String::from_utf8_lossy(code).into_owned()
    } else {
        format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            code[0], code[1], code[2], code[3]
        )
    }
}

// ---- the demonstration model ---------------------------------------------------------------

/// Builds a synthetic still-image container that exercises the entire modelled surface: a `grid`
/// primary over two coded tiles, an alpha auxiliary, a thumbnail, Exif and XMP metadata items, all
/// three `colr` forms, every transform property, HDR `clli`, an unrecognised property carried
/// verbatim, every reference type, and an `altr` entity group. Coded payloads are placeholders.
fn demo_image() -> Result<IsoBmffImage, CliError> {
    let av1c = || PropertyKind::CodecConfiguration {
        kind: *b"av1C",
        data: vec![0x81, 0x20, 0x0c, 0x00],
    };
    let ispe = |width, height| PropertyKind::ImageSpatialExtents { width, height };
    let pixi = |bits: &[u8]| PropertyKind::PixelInformation {
        bits_per_channel: bits.to_vec(),
    };
    let essential = |kind| Property {
        essential: true,
        kind,
    };
    let descriptive = |kind| Property {
        essential: false,
        kind,
    };

    // Primary: a grid derived image over two tiles, carrying the transform properties.
    let grid = Item {
        id: 1,
        item_type: *b"grid",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: false,
        references: vec![
            ItemReference {
                reference_type: *b"dimg",
                to_item_ids: vec![2, 3],
            },
            ItemReference {
                reference_type: *b"prem",
                to_item_ids: vec![4],
            },
        ],
        properties: vec![
            descriptive(ispe(256, 128)),
            essential(PropertyKind::Rotation(1)),
            essential(PropertyKind::Mirror(0)),
            essential(PropertyKind::CleanAperture {
                width_n: 256,
                width_d: 1,
                height_n: 128,
                height_d: 1,
                horiz_off_n: 0,
                horiz_off_d: 1,
                vert_off_n: 0,
                vert_off_d: 1,
            }),
            descriptive(PropertyKind::PixelAspectRatio {
                h_spacing: 1,
                v_spacing: 1,
            }),
        ],
        payload: ImageGrid {
            rows: 1,
            columns: 2,
            output_width: 256,
            output_height: 128,
        }
        .to_bytes()?,
    };

    // Tile A: nclx colour, plus an unrecognised `mdcv` property carried verbatim.
    let tile_a = Item {
        id: 2,
        item_type: *b"av01",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: true,
        references: vec![],
        properties: vec![
            essential(av1c()),
            descriptive(ispe(128, 128)),
            descriptive(pixi(&[8, 8, 8])),
            descriptive(PropertyKind::Colour(ColourInformation::Nclx(NclxColr {
                colour_primaries: 1,
                transfer_characteristics: 13,
                matrix_coefficients: 6,
                full_range: true,
            }))),
            descriptive(PropertyKind::Other {
                kind: *b"mdcv",
                data: vec![0; 24],
            }),
        ],
        payload: vec![0xde, 0xad, 0xbe, 0xef],
    };

    // Tile B: an unrestricted ICC profile in `colr`.
    let tile_b = Item {
        id: 3,
        item_type: *b"av01",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: true,
        references: vec![],
        properties: vec![
            essential(av1c()),
            descriptive(ispe(128, 128)),
            descriptive(pixi(&[8, 8, 8])),
            descriptive(PropertyKind::Colour(ColourInformation::UnrestrictedIcc(
                vec![0u8; 12],
            ))),
        ],
        payload: vec![0xca, 0xfe, 0xba, 0xbe],
    };

    // Alpha auxiliary for the grid, monochrome, essential auxC type.
    let alpha = Item {
        id: 4,
        item_type: *b"av01",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: true,
        references: vec![ItemReference {
            reference_type: *b"auxl",
            to_item_ids: vec![1],
        }],
        properties: vec![
            essential(av1c()),
            descriptive(ispe(256, 128)),
            descriptive(pixi(&[8])),
            essential(PropertyKind::AuxiliaryType {
                aux_type: "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha".to_string(),
                aux_subtype: Vec::new(),
            }),
        ],
        payload: vec![0x00, 0x11, 0x22, 0x33],
    };

    // Exif metadata item describing the primary.
    let exif = Item {
        id: 5,
        item_type: *b"Exif",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: false,
        references: vec![ItemReference {
            reference_type: *b"cdsc",
            to_item_ids: vec![1],
        }],
        properties: vec![],
        payload: b"\x00\x00\x00\x00II*\x00\x08\x00\x00\x00".to_vec(),
    };

    // XMP metadata item (a `mime` item with an RDF/XML content type).
    let xmp = Item {
        id: 6,
        item_type: *b"mime",
        name: String::new(),
        content_type: Some("application/rdf+xml".to_string()),
        content_encoding: None,
        hidden: false,
        references: vec![ItemReference {
            reference_type: *b"cdsc",
            to_item_ids: vec![1],
        }],
        properties: vec![],
        payload: b"<?xpacket?><x:xmpmeta/>".to_vec(),
    };

    // Thumbnail: a restricted ICC profile plus an HDR content-light-level property.
    let thumb = Item {
        id: 7,
        item_type: *b"av01",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden: true,
        references: vec![ItemReference {
            reference_type: *b"thmb",
            to_item_ids: vec![1],
        }],
        properties: vec![
            essential(av1c()),
            descriptive(ispe(64, 32)),
            descriptive(pixi(&[8, 8, 8])),
            descriptive(PropertyKind::Colour(ColourInformation::RestrictedIcc(
                vec![0u8; 12],
            ))),
            descriptive(PropertyKind::ContentLightLevel {
                max_content_light_level: 1000,
                max_pic_average_light_level: 400,
            }),
        ],
        payload: vec![0x44, 0x55, 0x66, 0x77],
    };

    Ok(IsoBmffImage::new(
        *b"avif",
        vec![*b"avif", *b"mif1", *b"miaf", *b"MA1A"],
        1,
        vec![grid, tile_a, tile_b, alpha, exif, xmp, thumb],
    )
    .with_groups(vec![EntityGroup {
        group_type: *b"altr",
        group_id: 1000,
        entity_ids: vec![1, 7],
    }]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_image_round_trips_through_write_read() {
        let img = demo_image().expect("demo image builds");
        let bytes = isobmff::write(&img).expect("write");
        let back = isobmff::read(&bytes).expect("read");
        assert_eq!(back, img);
    }

    #[test]
    fn demo_grid_payload_parses_back() {
        let img = demo_image().expect("demo image builds");
        let grid_item = img
            .items
            .iter()
            .find(|i| i.item_type == *b"grid")
            .expect("has a grid item");
        let g = ImageGrid::parse(&grid_item.payload).expect("grid payload parses");
        assert_eq!((g.rows, g.columns), (1, 2));
        assert_eq!((g.output_width, g.output_height), (256, 128));
    }
}
