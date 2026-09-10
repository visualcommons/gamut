//! Standard ancillary chunks (PNG spec §11.3): colour-space, physical, timing, and text metadata.
//!
//! These are optional. The encoder accumulates whatever the caller sets and emits the chunks in the
//! order PNG requires (Table 7): colour-space chunks before `PLTE`, the rest before `IDAT`.
//!
//! One chunk here is not PNG's own: the C2PA manifest store, `caBX` (C2PA 2.4 §A.3.2). It is
//! emitted **last** of everything before `IDAT`, so that its offset depends only on the chunks
//! that precede it and every byte after it is `IDAT` or `IEND` — which is what lets a reserved
//! store be filled in place ([`crate::fill_c2pa`]) without moving a byte outside the chunk.
//! §A.3.2 asks only that it precede `IDAT`.
//!
//! "Last" is this writer's guarantee about the files it produces, **not** a property that
//! survives other tools. PNG §14.3.2 is explicit that an unsafe-to-copy chunk's ordering
//! requirements are relative to the *critical* chunks only, that "it is never valid to assume
//! that a specific ancillary chunk type occurs with any particular positioning relative to other
//! ancillary chunks", and that a PNG editor may insert another ancillary chunk after one an
//! application always writes last. So a reader must assume no more than "before `IDAT`" — which
//! is exactly what [`crate::PngReport::c2pa`] and the decoder assume — while a *reservation*
//! whose offsets a signer depends on holds only for a file that has not been edited since this
//! encoder wrote it.
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

use gamut_core::{Error, Result};
use gamut_deflate::{DeflateEncoder, Level};

use crate::decoded::XMP_KEYWORD;
use crate::{ColorType, chunk};

/// The rendering intent for an `sRGB` chunk (PNG spec §11.3.2.5).
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
    /// `iTXt` with the compression flag set: zlib-compressed UTF-8.
    InternationalCompressed,
}

impl TextKind {
    /// The `iTXt` kind that carries the same compression choice.
    ///
    /// §11.3.3.2 sends text outside Latin-1's repertoire to `iTXt`, and §11.3.3.4 gives `iTXt` a
    /// compression flag of its own, so a promotion changes the character set and nothing else —
    /// a compressed annotation stays compressed.
    fn international(self) -> Self {
        match self {
            Self::Latin1 | Self::International => Self::International,
            Self::Compressed | Self::InternationalCompressed => Self::InternationalCompressed,
        }
    }
}

/// One accumulated text annotation, already **in the byte form its chunk carries**.
///
/// The distinction is the whole point of holding bytes rather than `String`s. PNG's three text
/// chunks do not share a character set: §11.3.3.1 restricts a keyword to Latin-1
/// ([ISO_8859-1]) in *every* one of them, §11.3.3.2 says a `tEXt` text string "is interpreted
/// according to the Latin-1 character set" (and §11.3.3.3 that inflating a `zTXt` "yields
/// Latin-1 text that is identical to the text that would be stored in an equivalent `tEXt`
/// chunk"), while §11.3.3.4 gives `iTXt` UTF-8. A Rust `String` is UTF-8, so writing its bytes
/// into a `tEXt` chunk stores mojibake for every code point above U+007F — `é` (U+00E9) becomes
/// the two bytes `C3 A9`, which a conforming reader shows as `Ã©`. Converting once, at the point
/// the caller sets the text, makes that unrepresentable: an entry's bytes are always already
/// right for its `kind`, or it carries the [`fault`](Self::fault) that stops it being written.
#[derive(Debug, Clone)]
struct TextEntry {
    /// The keyword, Latin-1 (§11.3.3.1). Empty when [`fault`](Self::fault) is set, because such
    /// an entry is never written — [`Ancillary::validate`] refuses the encode first.
    keyword: Vec<u8>,
    /// The text: Latin-1 for `tEXt`/`zTXt`, UTF-8 for `iTXt`.
    text: Vec<u8>,
    /// The `iTXt` language tag (§11.3.3.4, BCP 47); empty for the other kinds and for an
    /// unspecified language.
    language: Vec<u8>,
    /// The `iTXt` translated keyword (UTF-8, §11.3.3.4); empty for the other kinds.
    translated: Vec<u8>,
    kind: TextKind,
    /// Whether this entry came from [`Ancillary::begin_carry`] rather than a direct setter, so a
    /// second carry can replace exactly what the first contributed.
    carried: bool,
    /// Why this annotation must not be written, if it must not. Recorded here rather than
    /// returned from the setter because the setters sit behind `#[must_use]` builder methods
    /// that have no error channel; [`Ancillary::validate`] reports it at the encode chokepoint.
    fault: Option<TextFault>,
}

/// Why one accumulated text annotation cannot be written, and which annotation it was.
#[derive(Debug, Clone)]
struct TextFault {
    /// The keyword exactly as the caller gave it, for the refusal message — including a keyword
    /// that is itself the fault.
    keyword: String,
    /// The clause the annotation breaks, phrased for the caller.
    reason: &'static str,
}

/// §11.3.3.1: "Keywords are restricted to 1 to 79 bytes in length."
const KEYWORD_LENGTH: &str = "a keyword is restricted to 1 to 79 bytes (§11.3.3.1)";
/// §11.3.3.1: "Keywords shall contain only printable Latin-1 [ISO_8859-1] characters and spaces;
/// that is, only code points 0x20-7E and 0xA1-FF are allowed", and expressly "nor is U+00A0
/// NON-BREAKING SPACE". A null is outside it too, which is also §11.3.3.2's "Neither the keyword
/// nor the text string may contain a null character".
const KEYWORD_REPERTOIRE: &str = "a keyword may hold only code points 0x20-0x7E and 0xA1-0xFF \
     — no null, no control character, not U+00A0 (§11.3.3.1)";
/// §11.3.3.1: "leading spaces, trailing spaces, and consecutive spaces are not permitted in
/// keywords".
const KEYWORD_SPACES: &str =
    "a keyword may not have a leading, trailing or consecutive space (§11.3.3.1)";
/// §11.3.3.2 for `tEXt`/`zTXt` ("Neither the keyword nor the text string may contain a null
/// character") and §11.3.3.4 for `iTXt` ("neither shall contain a zero byte"). The null is the
/// field separator, so an embedded one does not merely offend the grammar — the chunk re-parses
/// as a *different* annotation.
const TEXT_NUL: &str = "a text string may not contain a null character (§11.3.3.2, §11.3.3.4)";
/// §11.3.3.4: "The language tag is a well-formed language tag defined by [BCP47]", whose subtags
/// are ASCII letters and digits joined by hyphens. Anything else is neither well-formed nor
/// (being written as UTF-8 and read back as Latin-1) byte-exact.
const LANGUAGE_TAG: &str =
    "an iTXt language tag may hold only ASCII letters, digits and '-' (§11.3.3.4, BCP 47)";
/// §11.3.3.4: "The translated keyword and text both use the UTF-8 encoding, and neither shall
/// contain a zero byte (null character)."
const TRANSLATED_NUL: &str =
    "an iTXt translated keyword may not contain a null character (§11.3.3.4)";
/// §11.3.3.4 gives the `iTXt` text field UTF-8 and no other encoding, so a packet that is not
/// UTF-8 has no chunk to go in. Dropping it silently is the loss this crate refuses to make.
const XMP_NOT_UTF8: &str =
    "the XMP packet is not UTF-8, and an iTXt text string must be (§11.3.3.4)";

/// Whether `c` is a printable Latin-1 character or a space, the repertoire §11.3.3.1 spells out
/// as "only code points 0x20-7E and 0xA1-FF".
fn printable_latin1(c: char) -> bool {
    matches!(u32::from(c), 0x20..=0x7E | 0xA1..=0xFF)
}

/// Whether `c` may appear in a `tEXt`/`zTXt` **text string**: §11.3.3.1's closing paragraph
/// restricts their content to "the printable Latin-1 character set plus U+000A LINE FEED (LF)".
///
/// §11.3.3.2 says more loosely that the text "may contain any Latin-1 character", which would
/// admit the C0/C1 controls and U+00A0. The tighter reading costs nothing to take: a character
/// outside this set is not rejected, it is *promoted* to `iTXt` — exactly what §11.3.3.2's own
/// "Text containing characters outside the repertoire of ISO/IEC 8859-1 should be encoded using
/// the iTXt chunk" directs — so the character always survives and only the chunk changes.
fn text_repertoire(c: char) -> bool {
    c == '\n' || printable_latin1(c)
}

/// The Latin-1 byte of `c`: Latin-1 is the first 256 Unicode code points, so the encoding is
/// `u8::try_from` — the exact inverse of the decoder's `latin1`, which maps byte *n* to U+00*nn*.
fn latin1_byte(c: char) -> Option<u8> {
    u8::try_from(u32::from(c)).ok()
}

/// The Latin-1 bytes of a keyword, or the §11.3.3.1 clause it breaks.
///
/// The repertoire is checked before the length so that the length bound counts *stored* bytes:
/// every character that passes is one Latin-1 byte, which a UTF-8 `str::len` is not.
fn keyword_bytes(keyword: &str) -> core::result::Result<Vec<u8>, &'static str> {
    let bytes: Option<Vec<u8>> = keyword
        .chars()
        .map(|c| latin1_byte(c).filter(|_| printable_latin1(c)))
        .collect();
    let bytes = bytes.ok_or(KEYWORD_REPERTOIRE)?;
    if bytes.is_empty() || bytes.len() > 79 {
        return Err(KEYWORD_LENGTH);
    }
    if keyword.starts_with(' ') || keyword.ends_with(' ') || keyword.contains("  ") {
        return Err(KEYWORD_SPACES);
    }
    Ok(bytes)
}

/// The Latin-1 bytes of a `tEXt`/`zTXt` text string, or `None` when a character is outside
/// [`text_repertoire`] — the signal to promote the annotation to `iTXt`.
fn text_bytes(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|c| latin1_byte(c).filter(|_| text_repertoire(c)))
        .collect()
}

/// The §11.3.3.4 clause an `iTXt`'s language tag or translated keyword breaks, if any.
fn itxt_field_fault(language: &str, translated: &str) -> Option<&'static str> {
    if !language
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Some(LANGUAGE_TAG);
    }
    translated.contains('\0').then_some(TRANSLATED_NUL)
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
    /// cICP: (colour primaries, transfer function, video full-range flag). The matrix
    /// coefficients byte is not carried because §11.3.2.6 fixes it at 0 for PNG.
    pub cicp: Option<(u8, u8, bool)>,
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
    /// caBX: the C2PA manifest store, raw and uncompressed (C2PA 2.4 §A.3.2) — or a run of zero
    /// bytes reserving its place. Emitted last, immediately before the first `IDAT`.
    pub c2pa: Option<Vec<u8>>,
    /// tEXt / zTXt / iTXt entries, emitted in insertion order.
    texts: Vec<TextEntry>,
    /// Whether the entries being pushed right now come from a metadata carry, so that a second
    /// carry can replace exactly what the first contributed. Set between [`Self::begin_carry`]
    /// and [`Self::end_carry`].
    carrying: bool,
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

    /// Adds an `iTXt` entry keeping its language tag and translated keyword (§11.3.3.4), which
    /// [`add_text_international`](Self::add_text_international) leaves empty, and its compression
    /// flag. Used to carry a decoded annotation forward without changing its identity: neither
    /// the two fields that make `iTXt` international nor the flag that keeps a 40-byte payload
    /// from being rewritten as 1600 uncompressed bytes.
    pub(crate) fn add_text_international_tagged(
        &mut self,
        keyword: &str,
        language: &str,
        translated: &str,
        text: &str,
        compressed: bool,
    ) {
        let kind = if compressed {
            TextKind::InternationalCompressed
        } else {
            TextKind::International
        };
        let mut entry = self.text_entry(keyword, text, kind);
        if entry.fault.is_none() {
            entry.fault = itxt_field_fault(language, translated).map(|reason| TextFault {
                keyword: keyword.to_string(),
                reason,
            });
        }
        entry.language = language.as_bytes().to_vec();
        entry.translated = translated.as_bytes().to_vec();
        self.texts.push(entry);
    }

    /// Adds an XMP packet as the `iTXt` §11.3.3.1 Table 21 reserves for it.
    ///
    /// Takes bytes rather than a `&str` because that is what the read side surfaces: a file's
    /// packet is whatever bytes its chunk held. §11.3.3.4 gives the `iTXt` text field UTF-8 and
    /// no alternative, so bytes that are not UTF-8 have no chunk to go in — and are recorded as
    /// a refusal rather than discarded, because a caller that handed this encoder a packet is
    /// entitled to learn it did not come out the other side.
    pub(crate) fn add_xmp(&mut self, packet: &[u8]) {
        match str::from_utf8(packet) {
            Ok(text) => self.add_text_international(XMP_KEYWORD, text),
            Err(_) => {
                let mut entry = self.text_entry(XMP_KEYWORD, "", TextKind::International);
                entry.fault = Some(TextFault {
                    keyword: XMP_KEYWORD.to_string(),
                    reason: XMP_NOT_UTF8,
                });
                self.texts.push(entry);
            }
        }
    }

    /// Starts carrying a read file's metadata, discarding whatever a previous carry contributed.
    ///
    /// This is what makes [`PngEncoder::with_metadata`](crate::PngEncoder::with_metadata)
    /// idempotent for text. The single-value slots — `gamma`, `iccp`, `srgb`, … — are idempotent
    /// already because a second write overwrites the first; the text list is the one place where
    /// "set it again" would otherwise mean "append it again", duplicating every annotation.
    pub(crate) fn begin_carry(&mut self) {
        self.texts.retain(|entry| !entry.carried);
        self.carrying = true;
    }

    /// Ends the carry started by [`begin_carry`](Self::begin_carry), so later direct setters push
    /// entries a subsequent carry will not remove.
    pub(crate) fn end_carry(&mut self) {
        self.carrying = false;
    }

    fn push_text(&mut self, keyword: &str, text: &str, kind: TextKind) {
        let entry = self.text_entry(keyword, text, kind);
        self.texts.push(entry);
    }

    /// Builds the entry for one text annotation, choosing the chunk that can actually carry it
    /// and recording the clause it breaks if no chunk can.
    ///
    /// The caller's `kind` is a *preference*, not a guarantee: §11.3.3.2 says outright that "text
    /// containing characters outside the repertoire of ISO/IEC 8859-1 should be encoded using the
    /// `iTXt` chunk", so a `tEXt`/`zTXt` request whose text leaves [`text_repertoire`] is
    /// promoted rather than written as bytes a Latin-1 reader mis-renders. The promotion keeps
    /// the caller's *other* choice, compression, because §11.3.3.4 gives `iTXt` a flag of its own.
    ///
    /// A null in the text is the one thing promotion cannot fix — §11.3.3.2 and §11.3.3.4 both
    /// forbid it, and it is the field separator, so the chunk would re-parse as a different
    /// annotation — and neither can a keyword outside §11.3.3.1's repertoire, length or spacing
    /// rules. Those become a [`TextFault`] the entry carries to [`Self::validate`].
    fn text_entry(&self, keyword: &str, text: &str, kind: TextKind) -> TextEntry {
        let (keyword_bytes, keyword_fault) = match keyword_bytes(keyword) {
            Ok(bytes) => (bytes, None),
            Err(reason) => (Vec::new(), Some(reason)),
        };
        let reason = keyword_fault.or_else(|| text.contains('\0').then_some(TEXT_NUL));
        // An iTXt was asked for as UTF-8 and stays UTF-8; only a Latin-1 request has a
        // repertoire to leave.
        let latin1 = match kind {
            TextKind::Latin1 | TextKind::Compressed => text_bytes(text),
            TextKind::International | TextKind::InternationalCompressed => None,
        };
        let (kind, text_bytes) = match latin1 {
            Some(bytes) => (kind, bytes),
            None => (kind.international(), text.as_bytes().to_vec()),
        };
        TextEntry {
            keyword: keyword_bytes,
            text: text_bytes,
            language: Vec::new(),
            translated: Vec::new(),
            kind,
            carried: self.carrying,
            fault: reason.map(|reason| TextFault {
                keyword: keyword.to_string(),
                reason,
            }),
        }
    }

    /// Refuses an accumulation the spec forbids, before any byte is emitted.
    ///
    /// Only the text chunks are refusable here, and only where a clause is a requirement rather
    /// than a recommendation: a keyword outside §11.3.3.1's repertoire, length or spacing rules;
    /// a null in a text string (§11.3.3.2, §11.3.3.4); a language tag or translated keyword
    /// §11.3.3.4 rules out; a non-UTF-8 XMP packet. Each is a chunk that would be *read back as
    /// something else* — the null re-frames the annotation outright — so writing it is a silent
    /// corruption, and dropping it is a silent loss.
    ///
    /// The colour chunks are deliberately **not** policed. §5.6 Table 5 and §11.3.2.5 say only
    /// that `sRGB` and `iCCP` "should not" appear together, and §15 gives the BCP 14 keywords
    /// force "when, and only when, they appear in all capitals"; §4.3 Table 1 then *presupposes*
    /// the co-occurrence and defines the outcome by ranking the chunks. Both are written, and a
    /// reader takes the highest-priority one.
    pub(crate) fn validate(&self) -> Result<()> {
        for (index, entry) in self.texts.iter().enumerate() {
            if let Some(fault) = &entry.fault {
                return Err(Error::invalid_input(
                    env!("CARGO_PKG_NAME"),
                    "PNG: a text annotation breaks the clause of the chunk that would carry it",
                )
                .with_detail(format!(
                    "text annotation {index} (keyword {:?}): {}",
                    fault.keyword, fault.reason
                )));
            }
        }
        Ok(())
    }

    /// Emits the colour-space chunks that must precede `PLTE` (PNG Table 7). `effort` is the
    /// encoder's [`Level::Best`] budget, applied to the compressed `iCCP` payload; `written` is
    /// the IHDR these chunks sit under, which `sBIT` must agree with.
    pub(crate) fn write_pre_plte(&self, out: &mut Vec<u8>, effort: u8, written: WrittenHeader<'_>) {
        if let Some((primaries, transfer, full_range)) = self.cicp {
            // §11.3.2.6 Table 18: primaries, transfer function, matrix coefficients, full-range
            // flag — one byte each, the matrix fixed at 0 because "RGB is currently the only
            // supported color model in PNG, and as such Matrix Coefficients shall be set to 0".
            chunk::write_chunk(
                out,
                *b"cICP",
                &[primaries, transfer, 0, u8::from(full_range)],
            );
        }
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

    /// Emits the remaining ancillary chunks that precede `IDAT` (after any `PLTE`/`tRNS`), the
    /// C2PA manifest store last of all so that it is the chunk immediately before `IDAT`.
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
        // Last, so nothing whose size could shift the store follows it: a reservation filled by
        // a second encode of equal length keeps every offset outside this chunk.
        if let Some(store) = &self.c2pa {
            chunk::write_chunk(out, chunk::CABX, store);
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

/// Serialises one text annotation. `entry.text` is already in the chunk's character set — Latin-1
/// for `tEXt`/`zTXt` (§11.3.3.2, §11.3.3.3), UTF-8 for `iTXt` (§11.3.3.4) — because
/// [`Ancillary::text_entry`] converted it when the caller set it, so this function only frames
/// the bytes.
fn write_text(out: &mut Vec<u8>, entry: &TextEntry, effort: u8) {
    let compress = |payload: &[u8], data: &mut Vec<u8>| {
        DeflateEncoder::new()
            .with_level(Level::Best)
            .with_effort(effort)
            .zlib_compress(payload, data);
    };
    let mut data = entry.keyword.clone();
    data.push(0); // null separator
    match entry.kind {
        TextKind::Latin1 => {
            data.extend_from_slice(&entry.text);
            chunk::write_chunk(out, *b"tEXt", &data);
        }
        TextKind::Compressed => {
            data.push(0); // compression method: 0 = zlib/deflate
            compress(&entry.text, &mut data);
            chunk::write_chunk(out, *b"zTXt", &data);
        }
        TextKind::International | TextKind::InternationalCompressed => {
            let compressed = entry.kind == TextKind::InternationalCompressed;
            data.push(u8::from(compressed)); // compression flag
            data.push(0); // compression method: 0 = zlib/deflate
            data.extend_from_slice(&entry.language);
            data.push(0); // language tag terminator
            data.extend_from_slice(&entry.translated);
            data.push(0); // translated keyword terminator
            if compressed {
                compress(&entry.text, &mut data);
            } else {
                data.extend_from_slice(&entry.text);
            }
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

    /// The manifest store is the last chunk the pre-IDAT pass writes, after every text entry
    /// added before or after it was set, and it is written raw: no keyword, no compression byte.
    #[test]
    fn the_c2pa_store_is_written_raw_and_last_before_idat() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Before", "set first");
        a.c2pa = Some(b"\0\0\0\x1fjumb".to_vec());
        a.add_text_compressed("After", "set later");
        a.set_time(2026, 9, 6, 0, 0, 0);
        let mut out = vec![0u8; 8];
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_EFFORT, RGB8);

        let mut types = Vec::new();
        let mut i = 8;
        while i + 12 <= out.len() {
            let len = u32::from_be_bytes([out[i], out[i + 1], out[i + 2], out[i + 3]]) as usize;
            types.push(out[i + 4..i + 8].to_vec());
            i += 12 + len;
        }
        assert_eq!(types.last().map(Vec::as_slice), Some(&b"caBX"[..]));
        assert_eq!(
            types.iter().filter(|t| t.as_slice() == b"caBX").count(),
            1,
            "exactly one store"
        );
        assert_eq!(
            find_chunk(&out, b"caBX"),
            Some(b"\0\0\0\x1fjumb".to_vec()),
            "the payload is the store verbatim"
        );
        // Unset, no chunk at all.
        let mut none = vec![0u8; 8];
        Ancillary::default().write_post_plte(&mut none, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        assert_eq!(find_chunk(&none, b"caBX"), None);
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

    /// Encodes `a`'s post-PLTE chunks and returns the buffer, so a claim can read the bytes a
    /// text annotation actually becomes.
    fn post_plte(a: &Ancillary) -> Vec<u8> {
        let mut out = vec![0u8; 8];
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        out
    }

    /// The refusal `validate` gives, rendered — including the owned detail naming the annotation.
    fn refusal(a: &Ancillary) -> String {
        a.validate().expect_err("the encode is refused").to_string()
    }

    /// A `tEXt` text string "is interpreted according to the Latin-1 character set" (§11.3.3.2),
    /// so a character above U+007F is **one** byte, not its UTF-8 pair.
    ///
    /// Kills a mutant of [`text_bytes`] that keeps the caller's `String` bytes: `é` would be
    /// stored as `C3 A9`, which a conforming reader renders `Ã©`. Asserted on the chunk payload
    /// rather than through a decode, because this crate's decoder maps Latin-1 back
    /// code-point-for-code-point and would agree with the encoder either way.
    #[test]
    fn latin1_text_is_written_one_byte_per_character() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Author", "café ÿ");
        assert_eq!(
            find_chunk(&post_plte(&a), b"tEXt"),
            Some(b"Author\0caf\xE9 \xFF".to_vec())
        );
    }

    /// §11.3.3.2: "Text containing characters outside the repertoire of ISO/IEC 8859-1 should be
    /// encoded using the iTXt chunk." A `tEXt` request whose text has no Latin-1 encoding is
    /// therefore promoted rather than mangled or dropped.
    ///
    /// Kills the `None` arm of [`Ancillary::text_entry`]'s promotion. The keyword stays Latin-1
    /// either way (§11.3.3.1 binds it in every text chunk).
    #[test]
    fn text_outside_latin1_is_promoted_to_itxt() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Title", "字");
        let out = post_plte(&a);
        assert_eq!(find_chunk(&out, b"tEXt"), None);
        // keyword, NUL, compression flag 0, method 0, empty language, empty translated keyword,
        // then the UTF-8 text (§11.3.3.4).
        assert_eq!(
            find_chunk(&out, b"iTXt"),
            Some(b"Title\0\0\0\0\0\xE5\xAD\x97".to_vec())
        );
    }

    /// §11.3.3.1 restricts a `tEXt`/`zTXt` text string to "the printable Latin-1 character set
    /// plus U+000A LINE FEED (LF)", and a control character is outside it — so it promotes, for
    /// the same reason a Han character does. The character survives either way; only the chunk
    /// that can define it changes.
    ///
    /// Kills [`text_repertoire`] mutated to accept everything Latin-1 can hold, which the looser
    /// wording of §11.3.3.2 ("may contain any Latin-1 character") would otherwise excuse. 0x7F
    /// DELETE is Latin-1-encodable and still not printable.
    #[test]
    fn a_control_character_promotes_the_annotation_to_itxt() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Title", "one\u{7F}two");
        let out = post_plte(&a);
        assert_eq!(find_chunk(&out, b"tEXt"), None);
        assert_eq!(
            find_chunk(&out, b"iTXt"),
            Some(b"Title\0\0\0\0\0one\x7Ftwo".to_vec())
        );
    }

    /// The other side of the same boundary: a line feed and the top of Latin-1 are *inside* the
    /// repertoire §11.3.3.1 grants `tEXt`, so neither promotes.
    ///
    /// Kills [`text_repertoire`] mutated to drop its `'\n'` case or to stop at 0xFE, either of
    /// which would push an ordinary multi-line Latin-1 note into an `iTXt`.
    #[test]
    fn a_line_feed_and_the_top_of_latin1_stay_in_a_text_chunk() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Description", "line\nÿ");
        let out = post_plte(&a);
        assert_eq!(find_chunk(&out, b"iTXt"), None);
        assert_eq!(
            find_chunk(&out, b"tEXt"),
            Some(b"Description\0line\n\xFF".to_vec())
        );
    }

    /// Promoting a `zTXt` keeps the caller's *compression*, because §11.3.3.4 gives `iTXt` a
    /// compression flag of its own — only the character set had to change.
    ///
    /// Kills the `Compressed` arm of [`TextKind::international`] and the compression-flag byte in
    /// [`write_text`]: a mutant that promotes to plain `International` leaves the flag at 0 and
    /// the body uncompressed.
    #[test]
    fn compressed_text_outside_latin1_stays_compressed_in_itxt() {
        let body = "字".repeat(200);
        let mut a = Ancillary::default();
        a.add_text_compressed("Comment", &body);
        let out = post_plte(&a);
        assert_eq!(find_chunk(&out, b"zTXt"), None);
        let itxt = find_chunk(&out, b"iTXt").expect("promoted to iTXt");
        assert_eq!(&itxt[..12], b"Comment\0\x01\0\0\0");
        assert!(
            itxt.len() < body.len(),
            "the body is deflated, not copied: {} bytes",
            itxt.len()
        );
    }

    /// §5.6 Table 5 and §11.3.2.5 say only that the two chunks "should not" appear together —
    /// lowercase, and §15 gives the BCP 14 keywords force "when, and only when, they appear in
    /// all capitals" — while §4.3 Table 1 presupposes the pair and ranks it. Both are written, so
    /// no colour information the caller supplied is thrown away.
    ///
    /// Kills a mutant that reinstates a refusal or drops one of the two chunks.
    #[test]
    fn a_profile_and_a_rendering_intent_are_both_written() {
        let mut a = Ancillary::default();
        a.set_srgb(SrgbIntent::Perceptual);
        a.iccp = Some(("prof".to_string(), vec![0u8; 4]));
        assert!(a.validate().is_ok(), "the pair is legal");

        let mut out = vec![0u8; 8];
        a.write_pre_plte(&mut out, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        assert_eq!(find_chunk(&out, b"sRGB"), Some(vec![0]));
        assert!(
            find_chunk(&out, b"iCCP").is_some(),
            "the profile is written"
        );
    }

    /// §11.3.3.1: "Keywords are restricted to 1 to 79 bytes in length." Both edges, because an
    /// empty keyword makes a third-party reader drop the whole annotation and an over-long one is
    /// a chunk no conforming reader has to accept.
    ///
    /// Kills the length guard in [`keyword_bytes`], including a mutant that shifts either bound
    /// by one.
    #[test]
    fn a_keyword_outside_one_to_seventy_nine_bytes_is_refused() {
        let mut ok = Ancillary::default();
        ok.add_text_latin1(&"k".repeat(79), "body");
        ok.add_text_latin1("k", "body");
        assert!(ok.validate().is_ok(), "79 bytes and 1 byte are inside");

        for keyword in ["", &"k".repeat(80)] {
            let mut a = Ancillary::default();
            a.add_text_latin1(keyword, "body");
            assert!(
                refusal(&a).contains("restricted to 1 to 79 bytes"),
                "keyword of {} bytes",
                keyword.len()
            );
        }
    }

    /// §11.3.3.1: "only code points 0x20-7E and 0xA1-FF are allowed", and expressly "nor is
    /// U+00A0 NON-BREAKING SPACE since it is visually indistinguishable from an ordinary space".
    /// The null is the same clause read through §11.3.3.2 — and the one that *corrupts* rather
    /// than merely offends, because it is the field separator: `Auth\0or` re-parses as the
    /// annotation `Auth`.
    ///
    /// Kills the repertoire guard in [`keyword_bytes`] and each edge of [`printable_latin1`].
    #[test]
    fn a_keyword_outside_the_printable_latin1_repertoire_is_refused() {
        for keyword in [
            "Auth\0or",   // the field separator itself
            "Auth\u{7F}", // DELETE
            "Auth\u{9F}", // C1 control
            "Auth\u{A0}", // NON-BREAKING SPACE, named by the clause
            "题",         // outside Latin-1 altogether
        ] {
            let mut a = Ancillary::default();
            a.add_text_latin1(keyword, "body");
            assert!(
                refusal(&a).contains("code points 0x20-0x7E and 0xA1-0xFF"),
                "keyword {keyword:?}"
            );
        }

        let mut edges = Ancillary::default();
        edges.add_text_latin1("a\u{20}b\u{7E}\u{A1}\u{FF}", "body");
        assert!(edges.validate().is_ok(), "0x20, 0x7E, 0xA1 and 0xFF are in");
    }

    /// §11.3.3.1: "leading spaces, trailing spaces, and consecutive spaces are not permitted in
    /// keywords", so that a keyword cannot be misread as another.
    ///
    /// Kills the spacing guard in [`keyword_bytes`], one condition at a time.
    #[test]
    fn a_keyword_with_a_leading_trailing_or_consecutive_space_is_refused() {
        for keyword in [" Author", "Author ", "Two  Words"] {
            let mut a = Ancillary::default();
            a.add_text_latin1(keyword, "body");
            assert!(
                refusal(&a).contains("leading, trailing or consecutive space"),
                "keyword {keyword:?}"
            );
        }

        let mut ok = Ancillary::default();
        ok.add_text_latin1("Two Words", "body");
        assert!(ok.validate().is_ok(), "a single interior space is allowed");
    }

    /// §11.3.3.2: "Neither the keyword nor the text string may contain a null character", and
    /// §11.3.3.4 the same for `iTXt`. This is corruption, not pedantry: the null is the field
    /// separator, so `note\0Author\0other` written as a `tEXt` body re-parses as a *different*
    /// annotation. Promotion cannot rescue it, because `iTXt` forbids it too.
    ///
    /// Kills the null guard in [`Ancillary::text_entry`], in both the Latin-1 and the UTF-8
    /// request — a mutant that checks only one leaves the other writing the corrupt chunk.
    #[test]
    fn a_null_in_a_text_string_is_refused() {
        let mut latin1 = Ancillary::default();
        latin1.add_text_latin1("Note", "before\0after");
        assert!(refusal(&latin1).contains("may not contain a null character"));

        let mut utf8 = Ancillary::default();
        utf8.add_text_international("Note", "before\0after");
        assert!(refusal(&utf8).contains("may not contain a null character"));
    }

    /// §11.3.3.4: "The translated keyword and text both use the UTF-8 encoding, and neither shall
    /// contain a zero byte (null character)" — the translated keyword is null-terminated too, so
    /// an embedded null re-frames everything after it.
    ///
    /// Kills the translated-keyword arm of [`itxt_field_fault`].
    #[test]
    fn a_null_in_a_translated_keyword_is_refused() {
        let mut a = Ancillary::default();
        a.add_text_international_tagged("Note", "de", "No\0tiz", "body", false);
        assert!(refusal(&a).contains("translated keyword may not contain a null"));
    }

    /// §11.3.3.4: "The language tag is a well-formed language tag defined by [BCP47]", whose
    /// subtags are ASCII letters and digits joined by hyphens. Anything else is not a tag, and —
    /// written as UTF-8 into a field a reader takes as Latin-1 — would not even survive the trip.
    ///
    /// Kills the language arm of [`itxt_field_fault`], and the empty case pins that "unspecified"
    /// stays legal.
    #[test]
    fn a_language_tag_outside_bcp_47_is_refused() {
        for language in ["de\0DE", "zh_Hans", "dé"] {
            let mut a = Ancillary::default();
            a.add_text_international_tagged("Note", language, "", "body", false);
            assert!(
                refusal(&a).contains("ASCII letters, digits and '-'"),
                "language {language:?}"
            );
        }

        let mut ok = Ancillary::default();
        ok.add_text_international_tagged("Note", "", "", "body", false);
        ok.add_text_international_tagged("Note", "ar-AE-u-nu-latn", "", "body", false);
        assert!(
            ok.validate().is_ok(),
            "empty and a full BCP 47 tag are fine"
        );
    }

    /// §11.3.3.4 gives the `iTXt` text field UTF-8 and no alternative, so a packet that is not
    /// UTF-8 has no chunk to go in. It is refused rather than quietly discarded: the read side
    /// surfaces a packet as raw bytes, and a caller that handed those bytes back is entitled to
    /// learn they did not come out the other side.
    ///
    /// Kills the `Err` arm of [`Ancillary::add_xmp`] — with it gone the packet vanishes silently.
    #[test]
    fn a_non_utf8_xmp_packet_is_refused() {
        let mut a = Ancillary::default();
        a.add_xmp(b"<x:xmpmeta \xFF\xFE/>");
        assert!(refusal(&a).contains("XMP packet is not UTF-8"));

        let mut valid = Ancillary::default();
        valid.add_xmp(b"<x:xmpmeta/>");
        assert!(valid.validate().is_ok(), "a UTF-8 packet is carried");
        assert!(find_chunk(&post_plte(&valid), b"iTXt").is_some());
    }

    /// A refusal a caller cannot act on is barely better than a silent drop, so it names *which*
    /// annotation offended — its position and its keyword, escaped so a null shows up.
    ///
    /// Kills the `enumerate` and the owned detail in [`Ancillary::validate`]: with either gone
    /// the message is the same for every annotation in the file.
    #[test]
    fn the_refusal_names_the_annotation_and_its_keyword() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Title", "fine");
        a.add_text_latin1("Author", "bad\0body");
        let message = refusal(&a);
        assert!(message.contains("text annotation 1"), "{message}");
        assert!(message.contains(r#""Author""#), "{message}");
    }

    /// §11.3.3.4's language tag and translated keyword survive, so carrying a decoded `iTXt`
    /// forward does not strip the two fields that make it international.
    ///
    /// Kills [`Ancillary::add_text_international_tagged`] and the two `extend_from_slice` calls
    /// for them in [`write_text`]: with either gone the payload is shorter and the tags empty.
    #[test]
    fn a_tagged_itxt_keeps_its_language_and_translated_keyword() {
        let mut a = Ancillary::default();
        a.add_text_international_tagged("Author", "de", "Autor", "gämut", false);
        assert_eq!(
            find_chunk(&post_plte(&a), b"iTXt"),
            Some(b"Author\0\0\0de\0Autor\0g\xC3\xA4mut".to_vec())
        );
    }

    /// A carry replaces what an earlier carry contributed instead of appending a second copy, so
    /// `with_metadata` is idempotent for text the way the single-value colour slots already are.
    ///
    /// Kills the `retain` in [`Ancillary::begin_carry`] (two copies of every annotation) and the
    /// `carried` flag's `end_carry` reset (a carry that also eats the caller's own annotations).
    #[test]
    fn a_second_carry_replaces_the_first_and_spares_direct_setters() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Before", "kept");
        a.begin_carry();
        a.add_text_latin1("Carried", "once");
        a.end_carry();
        // Set *after* the carry ended: it must not be mistaken for part of it, which is what
        // `end_carry` is for and what a mutant that skips it would get wrong.
        a.add_text_latin1("After", "kept");
        a.begin_carry();
        a.add_text_latin1("Carried", "once");
        a.end_carry();

        let keywords: Vec<&[u8]> = a.texts.iter().map(|e| e.keyword.as_slice()).collect();
        assert_eq!(
            keywords,
            [
                b"Before".as_slice(),
                b"After".as_slice(),
                b"Carried".as_slice()
            ]
        );
    }

    /// §11.3.2.6 Table 18 orders the payload primaries, transfer function, matrix coefficients,
    /// full-range flag — and fixes the matrix at 0 for PNG, so the setter has no argument for it.
    ///
    /// Kills the cICP arm of [`Ancillary::write_pre_plte`]: the two code points differ, so a
    /// mutant that swaps them fails, and the literal `0` is asserted in its own position.
    #[test]
    fn cicp_is_written_with_the_matrix_fixed_at_zero() {
        let full = Ancillary {
            cicp: Some((9, 16, true)),
            ..Default::default()
        };
        let mut out = vec![0u8; 8];
        full.write_pre_plte(&mut out, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        assert_eq!(find_chunk(&out, b"cICP"), Some(vec![9, 16, 0, 1]));

        let narrow = Ancillary {
            cicp: Some((1, 13, false)),
            ..Default::default()
        };
        let mut out = vec![0u8; 8];
        narrow.write_pre_plte(&mut out, DeflateEncoder::DEFAULT_EFFORT, RGB8);
        assert_eq!(find_chunk(&out, b"cICP"), Some(vec![1, 13, 0, 0]));
    }
}
