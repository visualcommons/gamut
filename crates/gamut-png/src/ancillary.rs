//! Standard ancillary chunks (PNG spec §11.3): colour-space, physical, timing, and text metadata.
//!
//! These are optional. The encoder accumulates whatever the caller sets and emits the chunks in the
//! order PNG requires (Table 7): colour-space chunks before `PLTE`, the rest before `IDAT`.
//!
//! Two of them, `bKGD` and `sBIT`, have a payload whose shape is the image's colour type, and the
//! encoder does not always write the colour type the caller set them for: auto-reduce may write a
//! palette, a greyscale or a colour-keyed truecolour image in place of the input's layout, and the
//! palette and colour-key candidates are *raced* against the unreduced encoding on compressed
//! size, so which one lands is not knowable when the chunk is set. Both are therefore emitted for
//! the header actually written — converted across colour types where a lossless conversion
//! exists, omitted otherwise ([`bkgd_for`], [`sbit_for`]) — rather than verbatim, because a
//! payload shaped for the wrong colour type is a chunk a reader rejects and drops.
//!
//! That contract holds across colour **types**. On the depth axis it is weaker: a `bKGD` sample is
//! checked against the written depth and omitted when out of range, but it is not *rescaled* when
//! auto-reduce demoted the samples (16→8 by `v / 257`, sub-byte grey by the depth's scale), so a
//! sample inside the written range keeps its input-depth value. That is issue #501, not this
//! module's claim.

use gamut_deflate::{DeflateEncoder, Level};

use crate::{ColorType, chunk};

/// The rendering intent for an `sRGB` chunk (PNG spec §11.3.3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrgbIntent {
    /// Perceptual (intent code 0).
    Perceptual,
    /// Relative colorimetric (intent code 1).
    RelativeColorimetric,
    /// Saturation (intent code 2).
    Saturation,
    /// Absolute colorimetric (intent code 3).
    AbsoluteColorimetric,
}

impl SrgbIntent {
    fn code(self) -> u8 {
        match self {
            SrgbIntent::Perceptual => 0,
            SrgbIntent::RelativeColorimetric => 1,
            SrgbIntent::Saturation => 2,
            SrgbIntent::AbsoluteColorimetric => 3,
        }
    }

    /// The intent for an sRGB chunk's code byte, or `None` if the code is not defined.
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(SrgbIntent::Perceptual),
            1 => Some(SrgbIntent::RelativeColorimetric),
            2 => Some(SrgbIntent::Saturation),
            3 => Some(SrgbIntent::AbsoluteColorimetric),
            _ => None,
        }
    }
}

/// The unit for a `pHYs` chunk's pixel dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalUnit {
    /// Unit is unknown; the values give only an aspect ratio (unit code 0).
    Unknown,
    /// Pixels per metre (unit code 1).
    Meter,
}

impl PhysicalUnit {
    fn code(self) -> u8 {
        match self {
            PhysicalUnit::Unknown => 0,
            PhysicalUnit::Meter => 1,
        }
    }
}

/// How a text chunk is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    /// `tEXt`: uncompressed Latin-1.
    Latin1,
    /// `zTXt`: zlib-compressed Latin-1.
    Compressed,
    /// `iTXt`: uncompressed UTF-8.
    International,
}

#[derive(Debug, Clone)]
struct TextEntry {
    keyword: String,
    text: String,
    kind: TextKind,
}

/// Accumulated ancillary metadata to emit alongside the image.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ancillary {
    /// gAMA: image gamma × 100000.
    pub gamma: Option<u32>,
    /// cHRM: white/red/green/blue x,y chromaticities × 100000 (8 values).
    pub chrm: Option<[u32; 8]>,
    /// sRGB: rendering-intent code.
    pub srgb: Option<u8>,
    /// sBIT: significant bits per channel (1–4 values, matching the colour type).
    pub sbit: Option<Vec<u8>>,
    /// bKGD: background colour, pre-serialised to its colour-type-specific bytes.
    pub bkgd: Option<Vec<u8>>,
    /// pHYs: (pixels-per-unit X, Y, unit code).
    pub phys: Option<(u32, u32, u8)>,
    /// tIME: year, month, day, hour, minute, second.
    pub time: Option<[u8; 7]>,
    /// iCCP: (profile name, raw ICC profile bytes); compressed at emit time.
    pub iccp: Option<(String, Vec<u8>)>,
    /// eXIf: raw EXIF/TIFF bytes (the chunk payload starts with the TIFF byte-order marker).
    pub exif: Option<Vec<u8>>,
    /// tEXt / zTXt / iTXt entries, emitted in insertion order.
    texts: Vec<TextEntry>,
}

impl Ancillary {
    pub(crate) fn set_srgb(&mut self, intent: SrgbIntent) {
        self.srgb = Some(intent.code());
    }

    pub(crate) fn set_physical(&mut self, x: u32, y: u32, unit: PhysicalUnit) {
        self.phys = Some((x, y, unit.code()));
    }

    pub(crate) fn set_time(&mut self, year: u16, month: u8, day: u8, hour: u8, min: u8, sec: u8) {
        let [yh, yl] = year.to_be_bytes();
        self.time = Some([yh, yl, month, day, hour, min, sec]);
    }

    pub(crate) fn add_text_latin1(&mut self, keyword: &str, text: &str) {
        self.push_text(keyword, text, TextKind::Latin1);
    }

    pub(crate) fn add_text_compressed(&mut self, keyword: &str, text: &str) {
        self.push_text(keyword, text, TextKind::Compressed);
    }

    pub(crate) fn add_text_international(&mut self, keyword: &str, text: &str) {
        self.push_text(keyword, text, TextKind::International);
    }

    fn push_text(&mut self, keyword: &str, text: &str, kind: TextKind) {
        self.texts.push(TextEntry {
            keyword: keyword.to_string(),
            text: text.to_string(),
            kind,
        });
    }

    /// Emits the colour-space chunks that must precede `PLTE` (PNG Table 7). `effort` is the
    /// encoder's [`Level::Best`] budget, applied to the compressed `iCCP` payload; `written` is
    /// the IHDR these chunks sit under, which `sBIT` must agree with.
    pub(crate) fn write_pre_plte(&self, out: &mut Vec<u8>, effort: u8, written: WrittenHeader<'_>) {
        if let Some(chrm) = self.chrm {
            let mut data = [0u8; 32];
            for (slot, value) in chrm.iter().enumerate() {
                data[slot * 4..slot * 4 + 4].copy_from_slice(&value.to_be_bytes());
            }
            chunk::write_chunk(out, *b"cHRM", &data);
        }
        if let Some(gamma) = self.gamma {
            chunk::write_chunk(out, *b"gAMA", &gamma.to_be_bytes());
        }
        if let Some((name, profile)) = &self.iccp {
            let mut data = name.clone().into_bytes();
            data.push(0); // null separator
            data.push(0); // compression method: 0 = zlib/deflate
            DeflateEncoder::new()
                .with_level(Level::Best)
                .with_effort(effort)
                .zlib_compress(profile, &mut data);
            chunk::write_chunk(out, *b"iCCP", &data);
        }
        if let Some(sbit) = self
            .sbit
            .as_deref()
            .and_then(|sbit| sbit_for(sbit, written.color, written.bit_depth))
        {
            chunk::write_chunk(out, *b"sBIT", &sbit);
        }
        if let Some(intent) = self.srgb {
            chunk::write_chunk(out, *b"sRGB", &[intent]);
        }
    }

    /// Emits the remaining ancillary chunks that precede `IDAT` (after any `PLTE`/`tRNS`).
    /// `effort` is the encoder's [`Level::Best`] budget, applied to compressed `zTXt` payloads;
    /// `written` is the IHDR (and palette) these chunks sit under, which `bKGD` must agree with.
    pub(crate) fn write_post_plte(
        &self,
        out: &mut Vec<u8>,
        effort: u8,
        written: WrittenHeader<'_>,
    ) {
        if let Some(exif) = &self.exif {
            chunk::write_chunk(out, *b"eXIf", exif);
        }
        if let Some(bkgd) = self
            .bkgd
            .as_deref()
            .and_then(|bkgd| bkgd_for(bkgd, written))
        {
            chunk::write_chunk(out, *b"bKGD", &bkgd);
        }
        if let Some((x, y, unit)) = self.phys {
            let mut data = [0u8; 9];
            data[0..4].copy_from_slice(&x.to_be_bytes());
            data[4..8].copy_from_slice(&y.to_be_bytes());
            data[8] = unit;
            chunk::write_chunk(out, *b"pHYs", &data);
        }
        if let Some(time) = self.time {
            chunk::write_chunk(out, *b"tIME", &time);
        }
        for entry in &self.texts {
            write_text(out, entry, effort);
        }
    }
}

/// Whose palette an indexed image is written with — which decides what a caller's palette
/// *index* refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteOrigin {
    /// The caller's own palette (`encode_indexed8`): an index the caller set names one of its
    /// entries.
    Caller,
    /// A palette the encoder derived from the pixels under auto-reduce, in an order the caller
    /// never saw (transparent entries first, then by luma): an index the caller set names nothing
    /// in it.
    Derived,
}

/// The palette an indexed image is written with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WrittenPalette<'a> {
    /// The `PLTE` payload: RGB triples.
    pub plte: &'a [u8],
    /// The `tRNS` payload — one alpha per leading entry, entries past its end being opaque
    /// (§11.3.2.1) — or `None` when every entry is opaque.
    pub trns: Option<&'a [u8]>,
    /// Whose palette it is.
    pub origin: PaletteOrigin,
}

impl WrittenPalette<'_> {
    /// The number of entries.
    fn len(self) -> usize {
        self.plte.len() / 3
    }

    /// Entry `index`'s alpha: its `tRNS` byte, or 255 past the end of `tRNS`.
    fn alpha(self, index: usize) -> u8 {
        self.trns
            .and_then(|trns| trns.get(index).copied())
            .unwrap_or(255)
    }

    /// The index of the entry holding `rgb`, preferring an opaque one.
    ///
    /// A background is a colour a viewer sees, so where a triple appears both as an opaque entry
    /// and as a transparent one — which the encoder's transparent-first ordering puts *first*,
    /// and which transparent cleanup manufactures whenever the image has opaque black — the
    /// opaque entry is the one meant. A triple that appears only under transparency still names
    /// that entry: its RGB is what a compositing reader paints.
    fn index_of(self, rgb: [u8; 3]) -> Option<usize> {
        let matches = || {
            self.plte
                .as_chunks::<3>()
                .0
                .iter()
                .enumerate()
                .filter(move |(_, entry)| **entry == rgb)
                .map(|(index, _)| index)
        };
        matches()
            .find(|&index| self.alpha(index) == 255)
            .or_else(|| matches().next())
    }
}

/// The IHDR — and, for an indexed image, the palette — the ancillary chunks are written under:
/// what a colour-type-shaped payload has to agree with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WrittenHeader<'a> {
    /// The colour type IHDR declares.
    pub color: ColorType,
    /// The bit depth IHDR declares.
    pub bit_depth: u8,
    /// The palette for [`ColorType::Indexed`]; `None` otherwise.
    pub palette: Option<WrittenPalette<'a>>,
}

impl WrittenHeader<'static> {
    /// A header without a palette — every colour type but [`ColorType::Indexed`].
    pub(crate) const fn new(color: ColorType, bit_depth: u8) -> Self {
        Self {
            color,
            bit_depth,
            palette: None,
        }
    }
}

/// The `bKGD` payload for the header actually written (§11.3.5.1), or `None` to omit the chunk.
///
/// The caller's payload names its own colour type by its length — one byte is a palette index,
/// two a grey sample, six an RGB triple, each sample 16-bit big-endian — and is converted where
/// the written header can carry the same colour losslessly:
///
/// - a grey sample and an RGB triple whose channels agree are the same colour, either way round;
/// - an RGB or grey colour under a palette becomes the index of the entry holding it — which
///   exists whenever the background colour occurs in the image, since the palette is built from
///   the image — preferring an opaque entry over a transparent twin of the same triple
///   ([`WrittenPalette::index_of`]), and is omitted when no entry does;
/// - a palette index names a colour only inside the palette the caller supplied. It is kept,
///   when in range, on the `encode_indexed8` path, whose palette is the caller's; under an
///   encoder-derived palette ([`PaletteOrigin::Derived`]) it names an entry in an order the
///   caller never saw, and under any other colour type there is no palette at all, so in both
///   cases it is omitted;
/// - a grey or RGB sample must fit the written depth (`value < 1 << depth` below 16 bits); one
///   that does not is omitted rather than written as a chunk the reader rejects.
///
/// The rules are the ones a reader applies before honouring the chunk — libpng's
/// `png_handle_bKGD` rejects a wrong length, an index past the palette and a sample past the
/// depth — so "converted or omitted" means "never dropped on read".
pub(crate) fn bkgd_for(bkgd: &[u8], written: WrittenHeader<'_>) -> Option<Vec<u8>> {
    let sample = |hi: u8, lo: u8| u16::from_be_bytes([hi, lo]);
    let rgb: [u16; 3] = match *bkgd {
        [index] => {
            // An index names an entry only in the palette the caller supplied.
            let palette = written.palette?;
            return (written.color == ColorType::Indexed
                && palette.origin == PaletteOrigin::Caller
                && usize::from(index) < palette.len())
            .then(|| vec![index]);
        }
        [hi, lo] => [sample(hi, lo); 3],
        [r1, r0, g1, g0, b1, b0] => [sample(r1, r0), sample(g1, g0), sample(b1, b0)],
        _ => return None,
    };
    match written.color {
        ColorType::Indexed => {
            let entry = rgb.map(|v| u8::try_from(v).ok());
            let entry = [entry[0]?, entry[1]?, entry[2]?];
            let index = written.palette?.index_of(entry)?;
            u8::try_from(index).ok().map(|index| vec![index])
        }
        ColorType::Grayscale | ColorType::GrayscaleAlpha => {
            let grey = (rgb[0] == rgb[1] && rgb[1] == rgb[2]).then_some(rgb[0])?;
            fits_depth(grey, written.bit_depth).then(|| grey.to_be_bytes().to_vec())
        }
        ColorType::Truecolor | ColorType::TruecolorAlpha => rgb
            .iter()
            .all(|&v| fits_depth(v, written.bit_depth))
            .then(|| rgb.iter().flat_map(|v| v.to_be_bytes()).collect()),
    }
}

/// Whether a 16-bit-framed `bKGD` sample is in range for the written depth: any value at 16 bits,
/// below `1 << depth` otherwise (libpng rejects `buf[0] != 0 || buf[1] >= 1 << bit_depth`).
fn fits_depth(value: u16, bit_depth: u8) -> bool {
    bit_depth >= 16 || u32::from(value) < 1u32 << bit_depth
}

/// The `sBIT` payload for the header actually written (§11.3.3.4), or `None` to omit the chunk.
///
/// The caller's payload names its own colour type by its length — one entry for grey, two for
/// grey+alpha, three for RGB (and for a palette, whose entries are RGB), four for RGBA — and is
/// converted where every channel the written image has is described:
///
/// - dropping a channel the written image no longer has is lossless — RGBA to RGB or to a palette
///   drops the alpha entry, RGB to grey keeps the one value the three agreed on;
/// - grey and RGB are interchangeable where the three RGB entries agree;
/// - an alpha entry cannot be invented, so a payload without one is omitted under an alpha
///   colour type — a case no reduction reaches, since reductions only drop channels.
///
/// Every entry must then be `1..=depth`, where a palette's depth is that of its 8-bit entries
/// (libpng rejects `buf[i] == 0 || buf[i] > maxbits`). An entry the written depth cannot hold is
/// omitted with the chunk: a claim of twelve significant bits over an image demoted to eight is
/// not one the file can carry.
pub(crate) fn sbit_for(sbit: &[u8], color: ColorType, bit_depth: u8) -> Option<Vec<u8>> {
    let (rgb, alpha) = match *sbit {
        [g] => ([g; 3], None),
        [g, a] => ([g; 3], Some(a)),
        [r, g, b] => ([r, g, b], None),
        [r, g, b, a] => ([r, g, b], Some(a)),
        _ => return None,
    };
    let grey = || (rgb[0] == rgb[1] && rgb[1] == rgb[2]).then_some(rgb[0]);
    let entries = match color {
        ColorType::Grayscale => vec![grey()?],
        ColorType::GrayscaleAlpha => vec![grey()?, alpha?],
        ColorType::Truecolor | ColorType::Indexed => rgb.to_vec(),
        ColorType::TruecolorAlpha => vec![rgb[0], rgb[1], rgb[2], alpha?],
    };
    let max_bits = if color == ColorType::Indexed {
        8
    } else {
        bit_depth
    };
    entries
        .iter()
        .all(|&bits| (1..=max_bits).contains(&bits))
        .then_some(entries)
}

/// Serialises one text chunk (tEXt / zTXt / iTXt).
fn write_text(out: &mut Vec<u8>, entry: &TextEntry, effort: u8) {
    match entry.kind {
        TextKind::Latin1 => {
            let mut data = entry.keyword.clone().into_bytes();
            data.push(0); // null separator
            data.extend_from_slice(entry.text.as_bytes());
            chunk::write_chunk(out, *b"tEXt", &data);
        }
        TextKind::Compressed => {
            let mut data = entry.keyword.clone().into_bytes();
            data.push(0); // null separator
            data.push(0); // compression method: 0 = zlib/deflate
            DeflateEncoder::new()
                .with_level(Level::Best)
                .with_effort(effort)
                .zlib_compress(entry.text.as_bytes(), &mut data);
            chunk::write_chunk(out, *b"zTXt", &data);
        }
        TextKind::International => {
            let mut data = entry.keyword.clone().into_bytes();
            data.push(0); // null separator
            data.push(0); // compression flag: 0 = uncompressed
            data.push(0); // compression method
            data.push(0); // empty language tag, then null
            data.push(0); // empty translated keyword, then null
            data.extend_from_slice(entry.text.as_bytes()); // UTF-8 text
            chunk::write_chunk(out, *b"iTXt", &data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header the pre-existing serialisation tests were written against: 8-bit truecolour.
    const RGB8: WrittenHeader<'static> = WrittenHeader::new(ColorType::Truecolor, 8);

    fn find_chunk(png: &[u8], ty: &[u8; 4]) -> Option<Vec<u8>> {
        // Walk the chunk stream (after the 8-byte signature) and return a chunk's data.
        let mut i = 8;
        while i + 12 <= png.len() {
            let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
            let kind = &png[i + 4..i + 8];
            if kind == ty {
                return Some(png[i + 8..i + 8 + len].to_vec());
            }
            i += 12 + len;
        }
        None
    }

    /// `PhysicalUnit::Unknown` is unit code 0 and `Meter` is 1 — the two are not interchangeable.
    ///
    /// `SrgbIntent::code` was pinned by the serialisation test below; `PhysicalUnit::code` was
    /// only ever called, never asserted, so it could return a constant 1 and nothing failed
    /// (#110). A pHYs chunk claiming metres for an image whose aspect ratio is unitless is a
    /// wrong file, not a wrong number: readers scale by it.
    #[test]
    fn physical_unit_codes_are_distinct() {
        let mut unitless = Ancillary::default();
        unitless.set_physical(300, 300, PhysicalUnit::Unknown);
        assert_eq!(unitless.phys, Some((300, 300, 0)));

        let mut metric = Ancillary::default();
        metric.set_physical(2835, 2835, PhysicalUnit::Meter);
        assert_eq!(metric.phys, Some((2835, 2835, 1)));
    }

    #[test]
    fn pre_plte_serialisation() {
        let a = Ancillary {
            gamma: Some(45455),
            srgb: Some(SrgbIntent::Perceptual.code()),
            sbit: Some(vec![5, 6, 5]),
            ..Default::default()
        };
        let mut out = vec![0u8; 8]; // fake signature
        a.write_pre_plte(&mut out, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        assert_eq!(
            find_chunk(&out, b"gAMA"),
            Some(45455u32.to_be_bytes().to_vec())
        );
        assert_eq!(find_chunk(&out, b"sRGB"), Some(vec![0]));
        assert_eq!(find_chunk(&out, b"sBIT"), Some(vec![5, 6, 5]));
    }

    #[test]
    fn post_plte_serialisation() {
        let mut a = Ancillary::default();
        a.set_physical(2835, 2835, PhysicalUnit::Meter);
        a.set_time(2026, 6, 13, 1, 2, 3);
        a.add_text_latin1("Title", "hi");
        let mut out = vec![0u8; 8];
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        let phys = find_chunk(&out, b"pHYs").unwrap();
        assert_eq!(&phys[0..4], 2835u32.to_be_bytes());
        assert_eq!(phys[8], 1); // metre
        assert_eq!(
            find_chunk(&out, b"tIME").unwrap(),
            vec![7, 234, 6, 13, 1, 2, 3]
        ); // 2026 = 0x07EA
        assert_eq!(find_chunk(&out, b"tEXt").unwrap(), b"Title\0hi".to_vec());
    }

    #[test]
    fn iccp_and_exif_framing() {
        let a = Ancillary {
            iccp: Some(("p".to_string(), b"the quick brown fox".to_vec())),
            exif: Some(vec![0x49, 0x49, 0x2A, 0x00]),
            ..Default::default()
        };
        let mut pre = vec![0u8; 8];
        a.write_pre_plte(&mut pre, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        let iccp = find_chunk(&pre, b"iCCP").unwrap();
        assert_eq!(&iccp[..2], b"p\0"); // profile name + null
        assert_eq!(iccp[2], 0); // compression method
        assert_eq!(iccp[3], 0x78); // zlib CMF byte begins the compressed profile

        let mut post = vec![0u8; 8];
        a.write_post_plte(&mut post, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        assert_eq!(
            find_chunk(&post, b"eXIf").unwrap(),
            vec![0x49, 0x49, 0x2A, 0x00]
        );
    }

    #[test]
    fn compressed_text_has_keyword_and_zlib_stream() {
        // The zlib stream's validity is cross-checked end-to-end via libpng in the oracle tests
        // (libpng decompresses zTXt on read); here we just check the framing.
        let mut a = Ancillary::default();
        a.add_text_compressed("Comment", "the quick brown fox");
        let mut out = vec![0u8; 8];
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        let data = find_chunk(&out, b"zTXt").unwrap();
        assert_eq!(&data[..8], b"Comment\0");
        assert_eq!(data[8], 0); // compression method
        assert_eq!(data[9], 0x78); // the zlib CMF byte begins the compressed text
    }

    fn header(color: ColorType, bit_depth: u8) -> WrittenHeader<'static> {
        WrittenHeader::new(color, bit_depth)
    }

    /// Three entries: red, a grey, blue.
    const PLTE: [u8; 9] = [200, 30, 60, 77, 77, 77, 20, 90, 220];

    fn palette(origin: PaletteOrigin, trns: Option<&'static [u8]>) -> WrittenHeader<'static> {
        WrittenHeader {
            color: ColorType::Indexed,
            bit_depth: 8,
            palette: Some(WrittenPalette {
                plte: &PLTE,
                trns,
                origin,
            }),
        }
    }

    /// The caller's own opaque palette, at index depth 8.
    fn indexed(bit_depth: u8) -> WrittenHeader<'static> {
        WrittenHeader {
            bit_depth,
            ..palette(PaletteOrigin::Caller, None)
        }
    }

    #[test]
    fn a_background_index_survives_only_inside_the_callers_palette() {
        assert_eq!(bkgd_for(&[2], indexed(2)), Some(vec![2]));
        assert_eq!(bkgd_for(&[3], indexed(2)), None, "past the palette");
        // An encoder-derived palette is in an order the caller never saw.
        assert_eq!(bkgd_for(&[2], palette(PaletteOrigin::Derived, None)), None);
        // And under any other colour type there is no palette at all.
        assert_eq!(bkgd_for(&[0], header(ColorType::TruecolorAlpha, 8)), None);
        assert_eq!(bkgd_for(&[0], header(ColorType::Grayscale, 8)), None);
    }

    #[test]
    fn a_colour_with_a_transparent_twin_names_the_opaque_entry() {
        // Two black entries: the transparent one first, as the encoder orders them.
        const BLACKS: [u8; 9] = [0, 0, 0, 0, 0, 0, 20, 90, 220];
        let twins = |trns: Option<&'static [u8]>| WrittenHeader {
            color: ColorType::Indexed,
            bit_depth: 8,
            palette: Some(WrittenPalette {
                plte: &BLACKS,
                trns,
                origin: PaletteOrigin::Derived,
            }),
        };
        assert_eq!(
            bkgd_for(&[0, 0, 0, 0, 0, 0], twins(Some(&[0]))),
            Some(vec![1])
        );
        // Past the end of tRNS every entry is opaque, so the first match is opaque and wins.
        assert_eq!(bkgd_for(&[0, 0, 0, 0, 0, 0], twins(None)), Some(vec![0]));
        // A triple that exists only under transparency still names that entry: its RGB is what a
        // compositing reader paints.
        assert_eq!(
            bkgd_for(&[0, 0, 0, 0, 0, 0], twins(Some(&[0, 0]))),
            Some(vec![0])
        );
        // Derivation is independent of the origin: an RGB colour resolves against either.
        assert_eq!(
            bkgd_for(&[0, 20, 0, 90, 0, 220], twins(Some(&[0]))),
            Some(vec![2])
        );
    }

    #[test]
    fn a_colour_under_a_palette_becomes_the_index_of_its_entry() {
        // RGB (20, 90, 220) is entry 2; grey 77 is entry 1; (1, 2, 3) is nowhere.
        assert_eq!(bkgd_for(&[0, 20, 0, 90, 0, 220], indexed(8)), Some(vec![2]));
        assert_eq!(bkgd_for(&[0, 77], indexed(8)), Some(vec![1]));
        assert_eq!(bkgd_for(&[0, 1, 0, 2, 0, 3], indexed(8)), None);
        // A 16-bit sample has no 8-bit palette entry.
        assert_eq!(bkgd_for(&[1, 0, 1, 0, 1, 0], indexed(8)), None);
    }

    #[test]
    fn grey_and_rgb_backgrounds_convert_where_the_channels_agree() {
        assert_eq!(
            bkgd_for(&[0, 77, 0, 77, 0, 77], header(ColorType::Grayscale, 8)),
            Some(vec![0, 77])
        );
        assert_eq!(
            bkgd_for(&[0, 77, 0, 77, 0, 78], header(ColorType::GrayscaleAlpha, 8)),
            None,
            "not a grey"
        );
        assert_eq!(
            bkgd_for(&[0, 77], header(ColorType::Truecolor, 8)),
            Some(vec![0, 77, 0, 77, 0, 77])
        );
        // Same colour type: byte for byte.
        assert_eq!(
            bkgd_for(&[0, 1, 0, 2, 0, 3], header(ColorType::TruecolorAlpha, 8)),
            Some(vec![0, 1, 0, 2, 0, 3])
        );
        // A wrong-length payload has no colour type at all.
        assert_eq!(bkgd_for(&[1, 2, 3], header(ColorType::Truecolor, 8)), None);
    }

    #[test]
    fn a_background_sample_must_fit_the_written_depth() {
        // 256 does not fit depth 8 in either framing; anything fits depth 16.
        assert_eq!(bkgd_for(&[1, 0], header(ColorType::Grayscale, 8)), None);
        assert_eq!(
            bkgd_for(&[1, 0], header(ColorType::Grayscale, 16)),
            Some(vec![1, 0])
        );
        assert_eq!(
            bkgd_for(&[0, 1, 0, 2, 1, 0], header(ColorType::Truecolor, 8)),
            None
        );
        // Sub-byte grey: 3 is the last code at depth 2, 4 is not one.
        assert_eq!(
            bkgd_for(&[0, 3], header(ColorType::Grayscale, 2)),
            Some(vec![0, 3])
        );
        assert_eq!(bkgd_for(&[0, 4], header(ColorType::Grayscale, 2)), None);
        assert!(fits_depth(255, 8));
        assert!(!fits_depth(256, 8));
        assert!(fits_depth(65535, 16));
    }

    #[test]
    fn significant_bits_follow_the_written_channels() {
        // Dropping a channel the written image no longer has.
        assert_eq!(
            sbit_for(&[5, 6, 5, 4], ColorType::Truecolor, 8),
            Some(vec![5, 6, 5])
        );
        assert_eq!(
            sbit_for(&[5, 6, 5, 4], ColorType::Indexed, 1),
            Some(vec![5, 6, 5]),
            "a palette's sBIT is three entries at any index depth"
        );
        assert_eq!(
            sbit_for(&[7, 7, 7, 4], ColorType::GrayscaleAlpha, 8),
            Some(vec![7, 4])
        );
        assert_eq!(sbit_for(&[7, 7, 7], ColorType::Grayscale, 8), Some(vec![7]));
        assert_eq!(sbit_for(&[7, 4], ColorType::Grayscale, 8), Some(vec![7]));
        // Grey to RGB where the channels agree, and never to a differing RGB.
        assert_eq!(sbit_for(&[7], ColorType::Truecolor, 8), Some(vec![7, 7, 7]));
        assert_eq!(sbit_for(&[5, 6, 5], ColorType::Grayscale, 8), None);
        // All three must agree, not any two: one agreeing pair is still not a grey.
        assert_eq!(sbit_for(&[5, 5, 6], ColorType::Grayscale, 8), None);
        assert_eq!(sbit_for(&[6, 5, 5], ColorType::GrayscaleAlpha, 8), None);
        // An alpha entry cannot be invented.
        assert_eq!(sbit_for(&[5, 6, 5], ColorType::TruecolorAlpha, 8), None);
        assert_eq!(sbit_for(&[7], ColorType::GrayscaleAlpha, 8), None);
        // Same colour type: byte for byte; a wrong length has no colour type.
        assert_eq!(
            sbit_for(&[5, 6, 5, 4], ColorType::TruecolorAlpha, 8),
            Some(vec![5, 6, 5, 4])
        );
        assert_eq!(sbit_for(&[], ColorType::Truecolor, 8), None);
        assert_eq!(sbit_for(&[1, 2, 3, 4, 5], ColorType::Truecolor, 8), None);
    }

    #[test]
    fn a_significant_bit_count_is_one_to_the_written_depth() {
        assert_eq!(sbit_for(&[8], ColorType::Grayscale, 8), Some(vec![8]));
        assert_eq!(
            sbit_for(&[9], ColorType::Grayscale, 8),
            None,
            "past the depth"
        );
        assert_eq!(
            sbit_for(&[0], ColorType::Grayscale, 8),
            None,
            "zero is not a count"
        );
        assert_eq!(sbit_for(&[12], ColorType::Grayscale, 16), Some(vec![12]));
        // A palette's entries are 8-bit whatever the index depth.
        assert_eq!(
            sbit_for(&[8, 8, 8], ColorType::Indexed, 1),
            Some(vec![8, 8, 8])
        );
        assert_eq!(sbit_for(&[9, 8, 8], ColorType::Indexed, 8), None);
        // Sub-byte grey: the count cannot exceed the depth.
        assert_eq!(sbit_for(&[2], ColorType::Grayscale, 2), Some(vec![2]));
        assert_eq!(sbit_for(&[3], ColorType::Grayscale, 2), None);
    }

    #[test]
    fn the_writers_emit_the_converted_chunk_or_none() {
        // The two `write_*` entry points route through the conversions rather than emitting the
        // stored bytes: a four-entry sBIT under a written palette comes out as three, and an RGB
        // background under a written greyscale it cannot name comes out not at all.
        let a = Ancillary {
            sbit: Some(vec![5, 6, 5, 4]),
            bkgd: Some(vec![0, 1, 0, 2, 0, 3]),
            ..Default::default()
        };
        let mut pre = vec![0u8; 8];
        a.write_pre_plte(&mut pre, DeflateEncoder::DEFAULT_EFFORT, indexed(8));
        assert_eq!(find_chunk(&pre, b"sBIT"), Some(vec![5, 6, 5]));

        let mut post = vec![0u8; 8];
        a.write_post_plte(
            &mut post,
            DeflateEncoder::DEFAULT_EFFORT,
            header(ColorType::Grayscale, 8),
        );
        assert_eq!(find_chunk(&post, b"bKGD"), None);
    }
}
