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
use crate::encoder::MetadataNotice;
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
    /// The keyword, Latin-1 (§11.3.3.1). Empty when the keyword had no Latin-1 encoding at all,
    /// which is also when [`emit`](Self::emit) is clear.
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
    /// Whether this entry is the XMP packet (§11.3.3.1 Table 21's reserved keyword). A file
    /// carries one packet, so setting it again replaces this entry rather than adding a second.
    xmp: bool,
    /// Whether the entry is written at all. A cleared flag keeps the entry in the list purely to
    /// carry its [`notices`](Self::notices) — a payload dropped in silence is the defect this
    /// module exists to remove.
    emit: bool,
    /// What §11.3.3 says about this annotation that the caller has to hear: a keyword no chunk
    /// can hold, or one written verbatim that deviates from a recommendation. Surfaced by
    /// [`PngEncoder::metadata_notices`](crate::PngEncoder::metadata_notices).
    notices: Vec<MetadataNotice>,
    /// Why this annotation must not be written *at all*, if it must not — a null in the keyword
    /// or in the `iTXt` translated keyword, the two fields a null separator ends, and nothing
    /// else. Recorded here rather than returned from the setter because the
    /// setters sit behind `#[must_use]` builder methods that have no error channel;
    /// [`Ancillary::validate`] reports it at the encode chokepoint.
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

/// §11.3.3.2 and §11.3.3.4 lay all three text chunks out as "Keyword … Null separator … ", so
/// the keyword is the field that *ends* at its first zero byte. An embedded null there does not
/// merely offend the grammar — the chunk re-parses as a *different* annotation, `Auth\0or`
/// becoming the keyword `Auth` with `or` for its text. It is the one thing here that makes a
/// file **mean** something else, and so the one thing that refuses the encode.
///
/// The *text string* is the opposite case and is **not** covered by this: §11.3.3.2 says of it
/// "The text string is not null-terminated (the length of the chunk defines the ending)", and
/// §11.3.3.4 "The text, unlike other textual data in this chunk, is not null-terminated; its
/// length is derived from the chunk length". A null there re-frames nothing, so it is reported
/// as [`MetadataNotice::TextStringNull`] instead.
const KEYWORD_NUL: &str = "a keyword may not contain a null character (§11.3.3.2, §11.3.3.4)";
/// §11.3.3.4: "The translated keyword and text both use the UTF-8 encoding, and neither shall
/// contain a zero byte (null character)." Null-terminated like the language tag, so an embedded
/// one re-frames every field after it.
const TRANSLATED_NUL: &str =
    "an iTXt translated keyword may not contain a null character (§11.3.3.4)";

/// Whether `c` is a printable Latin-1 character or a space, the repertoire §11.3.3.1 spells out
/// as "only code points 0x20-7E and 0xA1-FF".
fn printable_latin1(c: char) -> bool {
    matches!(u32::from(c), 0x20..=0x7E | 0xA1..=0xFF)
}

/// The Latin-1 byte of `c`: Latin-1 is the first 256 Unicode code points, so the encoding is
/// `u8::try_from` — the exact inverse of the decoder's `latin1`, which maps byte *n* to U+00*nn*.
fn latin1_byte(c: char) -> Option<u8> {
    u8::try_from(u32::from(c)).ok()
}

/// What §11.3.3.1 has to say about one keyword, resolved into what the writer does with it.
///
/// Three outcomes, because the clause mixes three kinds of statement and §15 gives them different
/// force ("when, and only when, they appear in all capitals"). Everything §11.3.3.1 says about a
/// keyword's *shape* is lowercase — "Keywords shall contain only printable Latin-1", "leading
/// spaces, trailing spaces, and consecutive spaces are not permitted", "Keywords are restricted
/// to 1 to 79 bytes" — so none of it is binding, and this crate's own reader accepts every shape
/// of keyword the length allows. What separates the outcomes is therefore not the wording but
/// the consequence:
///
/// - a **null** is the field separator, so the chunk re-parses as a different annotation. Refuse;
/// - a keyword **no chunk can hold** — outside Latin-1, or outside the 1–79 bytes all three
///   chunks fix — is one this crate's reader and libpng both *drop*, so writing it loses the
///   annotation with nothing said. Drop it here instead, and say so;
/// - anything else round-trips through this crate's reader byte for byte, so the keyword is
///   written exactly as it arrived and the deviation is reported. Refusing it would fail a
///   conversion over a file whose pixels are fine, and the only escape would be discarding all
///   of its metadata.
enum Keyword {
    /// Write these Latin-1 bytes, reporting the recommendation the keyword does not meet.
    Write(Vec<u8>, Option<MetadataNotice>),
    /// Do not write the annotation; report why.
    Drop(MetadataNotice),
    /// Refuse the encode: the keyword holds the field separator.
    Refuse,
}

/// Resolves `keyword` against §11.3.3.1.
///
/// Latin-1 representability is settled before the length so that the bound counts *stored*
/// bytes: every character that passes is one Latin-1 byte, which a UTF-8 `str::len` is not.
fn keyword_verdict(keyword: &str) -> Keyword {
    if keyword.contains('\0') {
        return Keyword::Refuse;
    }
    let Some(bytes) = keyword
        .chars()
        .map(latin1_byte)
        .collect::<Option<Vec<u8>>>()
    else {
        return Keyword::Drop(MetadataNotice::TextKeywordNotLatin1);
    };
    if bytes.is_empty() || bytes.len() > 79 {
        return Keyword::Drop(MetadataNotice::TextKeywordLength);
    }
    if !keyword.chars().all(printable_latin1) {
        return Keyword::Write(bytes, Some(MetadataNotice::TextKeywordRepertoire));
    }
    let spacing = keyword.starts_with(' ') || keyword.ends_with(' ') || keyword.contains("  ");
    Keyword::Write(bytes, spacing.then_some(MetadataNotice::TextKeywordSpacing))
}

/// The Latin-1 bytes of a `tEXt`/`zTXt` text string, or `None` when a character has no Latin-1
/// encoding at all — the signal to promote the annotation to `iTXt`.
///
/// **The specification contradicts itself here, and the more specific clause wins.**
/// §11.3.3.1's closing paragraph says of `tEXt`/`zTXt` that "There are also tEXt and zTXt chunks,
/// whose content is restricted to the printable Latin-1 character set plus U+000A LINE FEED
/// (LF)". §11.3.3.2, the clause that defines `tEXt` itself, says the opposite one sentence after
/// naming the same character set: "Text is interpreted according to the Latin-1 character set
/// [ISO_8859-1]. The text string may contain any Latin-1 character." — adding only that
/// "Characters other than those defined in Latin-1 plus the linefeed character have no defined
/// meaning in tEXt chunks", which is a statement about characters *outside* Latin-1, not inside
/// it. §11.3.3.2 is the more specific and the more permissive of the two, so it is the one taken:
/// every Latin-1 character is written into the chunk that already interprets it as Latin-1, and
/// only a character Latin-1 cannot encode promotes to `iTXt` — which is what §11.3.3.2 itself
/// directs ("Text containing characters outside the repertoire of ISO/IEC 8859-1 should be
/// encoded using the iTXt chunk").
fn text_bytes(text: &str) -> Option<Vec<u8>> {
    text.chars().map(latin1_byte).collect()
}

/// Whether `language` has the shape §11.3.3.4 requires: "The language tag is a well-formed
/// language tag defined by [BCP47]", whose subtags are ASCII letters and digits joined by
/// hyphens. This checks the character set, not full BCP 47 well-formedness.
fn well_formed_language(language: &str) -> bool {
    language
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
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
        let entry = self.itxt_entry(keyword, language, translated, text, compressed);
        self.texts.push(entry);
    }

    /// Builds one `iTXt` entry with its §11.3.3.4 fields, shared by the tagged text setter and
    /// the XMP packet.
    ///
    /// A language tag outside §11.3.3.4's ASCII shape is **dropped, not refused**: written as
    /// UTF-8 into a field a reader takes as Latin-1 it would not survive the trip, but the
    /// annotation itself would, and an unspecified language is what §11.3.3.4 already means by
    /// an empty tag. A null in the translated keyword is a different thing — it re-frames every
    /// field after it — so it refuses, like every other null.
    fn itxt_entry(
        &self,
        keyword: &str,
        language: &str,
        translated: &str,
        text: &str,
        compressed: bool,
    ) -> TextEntry {
        let kind = if compressed {
            TextKind::InternationalCompressed
        } else {
            TextKind::International
        };
        let mut entry = self.text_entry(keyword, text, kind);
        if entry.fault.is_none() && translated.contains('\0') {
            entry.fault = Some(TextFault {
                keyword: keyword.to_string(),
                reason: TRANSLATED_NUL,
            });
        }
        if well_formed_language(language) {
            entry.language = language.as_bytes().to_vec();
        } else {
            entry.notices.push(MetadataNotice::ItxtLanguageTag);
        }
        entry.translated = translated.as_bytes().to_vec();
        entry
    }

    /// Adds an XMP packet as the `iTXt` §11.3.3.1 Table 21 reserves for it, framed the way the
    /// file that carried it framed it (§11.3.3.4's compression flag, language tag and translated
    /// keyword).
    ///
    /// Replaces any packet already accumulated rather than adding a second: a PNG carries one
    /// XMP packet, so this is a single-value payload like `iCCP` or `eXIf`, and two `iTXt` chunks
    /// under the same reserved keyword would leave a reader to pick — this crate's own reader
    /// keeps the first and discards the rest.
    ///
    /// Takes bytes rather than a `&str` because that is what the read side surfaces: a file's
    /// packet is whatever bytes its chunk held. §11.3.3.4 gives the `iTXt` text field UTF-8 and
    /// no alternative, so bytes that are not UTF-8 have no chunk to go in — and are reported
    /// rather than discarded, because a caller that handed this encoder a packet is entitled to
    /// learn it did not come out the other side.
    pub(crate) fn add_xmp(
        &mut self,
        packet: &[u8],
        language: &str,
        translated: &str,
        compressed: bool,
    ) {
        self.texts.retain(|entry| !entry.xmp);
        let mut entry = match str::from_utf8(packet) {
            Ok(text) => self.itxt_entry(XMP_KEYWORD, language, translated, text, compressed),
            Err(_) => {
                let mut entry = self.text_entry(XMP_KEYWORD, "", TextKind::International);
                entry.emit = false;
                entry.notices.push(MetadataNotice::XmpNotUtf8);
                entry
            }
        };
        entry.xmp = true;
        self.texts.push(entry);
    }

    /// Every §11.3.3 deviation the accumulated annotations carry, in insertion order.
    ///
    /// An entry that is **not emitted** reports only the notices that explain the drop.
    /// [`MetadataNotice::carried`](crate::MetadataNotice::carried) is fixed by the variant, so
    /// the entry's [`emit`](TextEntry::emit) flag is the authority on whether anything reached
    /// the output: one entry can record both a keyword no chunk can hold and a language tag that
    /// did not survive, and surfacing the second unfiltered would tell a caller its annotation
    /// came along with a caveat when no chunk was written at all.
    pub(crate) fn text_notices(&self) -> impl Iterator<Item = MetadataNotice> + '_ {
        self.texts.iter().flat_map(|entry| {
            entry
                .notices
                .iter()
                .copied()
                .filter(move |notice| entry.emit || !notice.carried())
        })
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
    /// A null is forbidden in both fields (§11.3.3.2, §11.3.3.4), and the two are not the same
    /// kind of forbidden. The **keyword** ends at its first null, so a null there makes the chunk
    /// re-parse as a *different* annotation and nothing can undo it: it becomes a [`TextFault`]
    /// the entry carries to [`Self::validate`], and the encode refuses. The **text string** is
    /// last and length-delimited — see [`KEYWORD_NUL`] for both clauses — so a null there
    /// re-frames nothing, and this crate's own reader hands such a text back intact; refusing it
    /// would make the writer stricter than its own reader over a file the reader accepts. It is
    /// not written either, because readers disagree about what the chunk then holds (libpng
    /// truncates the text at the null), so the annotation is dropped and reported as
    /// [`MetadataNotice::TextStringNull`]. Every *other* way a keyword can fall short of
    /// §11.3.3.1 is a [`MetadataNotice`] too: see [`Keyword`] for where that line falls.
    fn text_entry(&self, keyword: &str, text: &str, kind: TextKind) -> TextEntry {
        let (keyword_bytes, mut emit, notice, keyword_nul) = match keyword_verdict(keyword) {
            Keyword::Write(bytes, notice) => (bytes, true, notice, false),
            Keyword::Drop(notice) => (Vec::new(), false, Some(notice), false),
            Keyword::Refuse => (Vec::new(), true, None, true),
        };
        let mut notices: Vec<MetadataNotice> = notice.into_iter().collect();
        if text.contains('\0') {
            emit = false;
            notices.push(MetadataNotice::TextStringNull);
        }
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
            xmp: false,
            emit,
            notices,
            fault: keyword_nul.then(|| TextFault {
                keyword: keyword.to_string(),
                reason: KEYWORD_NUL,
            }),
        }
    }

    /// Refuses an accumulation the spec forbids, before any byte is emitted.
    ///
    /// **Only a null in a keyword gets here.** A null in a text chunk's keyword or in an `iTXt`
    /// translated keyword (§11.3.3.2, §11.3.3.4) sits in a field a null separator *ends*, so a
    /// chunk carrying one re-parses as a *different* annotation: the file would mean something
    /// other than what the caller supplied, and no notice can undo that. Everything else §11.3.3
    /// asks of a text chunk — the keyword's repertoire, length and spacing, a null in the
    /// length-delimited text string, the `iTXt` language tag's shape, an
    /// XMP packet that is not UTF-8 — is reported through
    /// [`PngEncoder::metadata_notices`](crate::PngEncoder::metadata_notices) and the encode
    /// proceeds. Refusing those would fail a conversion over a file whose pixels are fine, and
    /// leave the caller no way out but to discard all of its metadata, colour profile included.
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
        // An entry with `emit` clear is a placeholder holding its notice, not a chunk.
        for entry in self.texts.iter().filter(|entry| entry.emit) {
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

    /// The notices `a` has accumulated, in order.
    fn notices(a: &Ancillary) -> Vec<MetadataNotice> {
        a.text_notices().collect()
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

    /// The specification contradicts itself about a `tEXt` text string, and the more specific and
    /// more permissive clause is the one taken. §11.3.3.1's closing paragraph says `tEXt`/`zTXt`
    /// "content is restricted to the printable Latin-1 character set plus U+000A LINE FEED (LF)";
    /// §11.3.3.2, which *defines* `tEXt`, says "The text string may contain any Latin-1
    /// character". A control character, a line feed and the top of Latin-1 are therefore all
    /// written into the chunk that already interprets its bytes as Latin-1.
    ///
    /// Kills [`text_bytes`] mutated to filter its characters against a narrower repertoire, which
    /// would promote a conforming annotation to a different chunk type — changing the file's
    /// shape over a clause the spec itself contradicts.
    #[test]
    fn every_latin1_character_stays_in_a_text_chunk() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Description", "one\u{7F}two\nÿ\u{A0}");
        let out = post_plte(&a);
        assert_eq!(find_chunk(&out, b"iTXt"), None);
        assert_eq!(
            find_chunk(&out, b"tEXt"),
            Some(b"Description\0one\x7Ftwo\n\xFF\xA0".to_vec())
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
    /// empty keyword makes a reader drop the whole annotation and an over-long one is a chunk no
    /// conforming reader has to accept — including this crate's own, which splits a payload at
    /// its first null and refuses a keyword field outside 1–79 bytes. Writing such a chunk would
    /// therefore lose the annotation without a word, so it is dropped here and reported.
    ///
    /// Kills the length guard in [`keyword_verdict`], including a mutant that shifts either bound
    /// by one, and the `Drop` arm of [`Ancillary::text_entry`] that keeps the chunk out.
    #[test]
    fn a_keyword_outside_one_to_seventy_nine_bytes_is_dropped_with_a_notice() {
        let mut ok = Ancillary::default();
        ok.add_text_latin1(&"k".repeat(79), "body");
        ok.add_text_latin1("k", "body");
        assert!(notices(&ok).is_empty(), "79 bytes and 1 byte are inside");
        assert!(find_chunk(&post_plte(&ok), b"tEXt").is_some());

        for keyword in ["", &"k".repeat(80)] {
            let mut a = Ancillary::default();
            a.add_text_latin1(keyword, "body");
            assert_eq!(
                notices(&a),
                [MetadataNotice::TextKeywordLength],
                "keyword of {} bytes",
                keyword.len()
            );
            assert!(a.validate().is_ok(), "reported, not refused");
            assert_eq!(find_chunk(&post_plte(&a), b"tEXt"), None);
        }
    }

    /// §11.3.3.1 binds a keyword to Latin-1 in all three text chunks, so a character Latin-1
    /// cannot encode has no chunk to go in — unlike a *text string*, which §11.3.3.2 routes to
    /// `iTXt`. The annotation is dropped and reported rather than transliterated.
    ///
    /// Kills the `Drop(TextKeywordNotLatin1)` arm of [`keyword_verdict`]: with it gone the
    /// keyword's UTF-8 bytes reach a field a reader takes as Latin-1.
    #[test]
    fn a_keyword_outside_latin1_is_dropped_with_a_notice() {
        let mut a = Ancillary::default();
        a.add_text_latin1("题", "body");
        assert_eq!(notices(&a), [MetadataNotice::TextKeywordNotLatin1]);
        assert!(a.validate().is_ok(), "reported, not refused");
        assert_eq!(find_chunk(&post_plte(&a), b"tEXt"), None);
    }

    /// §11.3.3.1: "only code points 0x20-7E and 0xA1-FF are allowed", and expressly "nor is
    /// U+00A0 NON-BREAKING SPACE since it is visually indistinguishable from an ordinary space".
    /// Lowercase "shall", so §15 makes it advisory, and this crate's reader returns such a
    /// keyword unchanged — so the keyword is **written verbatim** and the deviation reported.
    ///
    /// Kills each edge of [`printable_latin1`] and the `Write(_, Some(..))` arm of
    /// [`keyword_verdict`]: a mutant that stops noticing leaves the caller unwarned, and one that
    /// drops the annotation loses metadata the file had.
    #[test]
    fn a_keyword_outside_the_printable_latin1_repertoire_is_written_with_a_notice() {
        for keyword in [
            "Auth\u{7F}or", // DELETE
            "Auth\u{9F}or", // C1 control
            "Auth\u{A0}or", // NON-BREAKING SPACE, named by the clause
        ] {
            let mut a = Ancillary::default();
            a.add_text_latin1(keyword, "body");
            assert_eq!(
                notices(&a),
                [MetadataNotice::TextKeywordRepertoire],
                "keyword {keyword:?}"
            );
            let mut expected = keyword.chars().map(|c| c as u8).collect::<Vec<u8>>();
            expected.extend_from_slice(b"\0body");
            assert_eq!(find_chunk(&post_plte(&a), b"tEXt"), Some(expected));
        }

        let mut edges = Ancillary::default();
        edges.add_text_latin1("a\u{20}b\u{7E}\u{A1}\u{FF}", "body");
        assert!(
            notices(&edges).is_empty(),
            "0x20, 0x7E, 0xA1 and 0xFF are in"
        );
    }

    /// §11.3.3.1: "leading spaces, trailing spaces, and consecutive spaces are not permitted in
    /// keywords", so that a keyword cannot be misread as another. Lowercase again, and again a
    /// keyword this crate's reader hands back unchanged, so it is written and reported.
    ///
    /// Kills the spacing guard in [`keyword_verdict`], one condition at a time.
    #[test]
    fn a_keyword_with_a_leading_trailing_or_consecutive_space_is_written_with_a_notice() {
        for keyword in [" Author", "Author ", "Two  Words"] {
            let mut a = Ancillary::default();
            a.add_text_latin1(keyword, "body");
            assert_eq!(
                notices(&a),
                [MetadataNotice::TextKeywordSpacing],
                "keyword {keyword:?}"
            );
            let mut expected = keyword.as_bytes().to_vec();
            expected.extend_from_slice(b"\0body");
            assert_eq!(find_chunk(&post_plte(&a), b"tEXt"), Some(expected));
        }

        let mut ok = Ancillary::default();
        ok.add_text_latin1("Two Words", "body");
        assert!(
            notices(&ok).is_empty(),
            "a single interior space is allowed"
        );
    }

    /// A null in the *keyword* is the field separator, so `Auth\0or` re-parses as the annotation
    /// `Auth` with `or` for its text: the chunk means something the caller never wrote. That —
    /// and only that — still refuses, which is the line between what this module reports and what
    /// it rejects.
    ///
    /// Kills the `Refuse` arm of [`keyword_verdict`], which no notice test can reach.
    #[test]
    fn a_null_in_a_keyword_is_refused() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Auth\0or", "body");
        assert!(refusal(&a).contains("may not contain a null character"));
    }

    /// §11.3.3.2 forbids a null in the text string ("Neither the keyword nor the text string may
    /// contain a null character") and §11.3.3.4 says the same for `iTXt` — but neither field is
    /// *framed* by a null: "The text string is not null-terminated (the length of the chunk
    /// defines the ending)". So the annotation is **dropped and reported**, not refused: this
    /// crate's own reader hands such a text back whole, and a writer must not fail on a file its
    /// own reader accepts. It is not written either, because libpng truncates such a text at the
    /// null, so the chunk would mean different things to different readers.
    ///
    /// Kills the text-null guard in [`Ancillary::text_entry`], in both the Latin-1 and the UTF-8
    /// request — a mutant that checks only one leaves the other writing the disputed chunk — and
    /// a mutant that turns the drop back into a refusal.
    #[test]
    fn a_null_in_a_text_string_is_dropped_with_a_notice() {
        let mut latin1 = Ancillary::default();
        latin1.add_text_latin1("Note", "before\0after");
        assert_eq!(notices(&latin1), [MetadataNotice::TextStringNull]);
        assert!(latin1.validate().is_ok(), "reported, not refused");
        assert_eq!(find_chunk(&post_plte(&latin1), b"tEXt"), None);

        let mut utf8 = Ancillary::default();
        utf8.add_text_international("Note", "before\0after");
        assert_eq!(notices(&utf8), [MetadataNotice::TextStringNull]);
        assert!(utf8.validate().is_ok(), "reported, not refused");
        assert_eq!(find_chunk(&post_plte(&utf8), b"iTXt"), None);
    }

    /// A notice that says the annotation *was written* has no business being reported for one no
    /// chunk carries. [`MetadataNotice::carried`] is fixed by the variant, so the entry's `emit`
    /// flag is what decides: an `iTXt` whose keyword no chunk can hold records the language tag's
    /// deviation on the same entry, and reporting that unfiltered tells a caller its annotation
    /// came along with a caveat when zero chunks were written.
    ///
    /// Kills the `emit` filter in [`Ancillary::text_notices`]; the second half pins that the
    /// filter does not swallow the same notice when the annotation *is* written.
    #[test]
    fn a_dropped_annotation_reports_only_why_it_was_dropped() {
        let mut dropped = Ancillary::default();
        dropped.add_text_international_tagged("\u{153}kw", "zh_Hans", "", "body", false);
        assert_eq!(
            notices(&dropped),
            [MetadataNotice::TextKeywordNotLatin1],
            "the language tag of an annotation nobody wrote is not news"
        );
        assert_eq!(find_chunk(&post_plte(&dropped), b"iTXt"), None);

        let mut written = Ancillary::default();
        written.add_text_international_tagged("kw", "zh_Hans", "", "body", false);
        assert_eq!(notices(&written), [MetadataNotice::ItxtLanguageTag]);
        assert!(find_chunk(&post_plte(&written), b"iTXt").is_some());
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
    /// subtags are ASCII letters and digits joined by hyphens. Anything else, written as UTF-8
    /// into a field a reader takes as Latin-1, would not survive the trip — so the **tag** goes
    /// and the annotation stays, an empty tag being §11.3.3.4's own way of saying the language is
    /// unspecified.
    ///
    /// Kills the language arm of [`Ancillary::itxt_entry`]; the empty case pins that
    /// "unspecified" is not itself a deviation.
    #[test]
    fn a_language_tag_outside_bcp_47_is_dropped_with_a_notice() {
        for language in ["de\0DE", "zh_Hans", "dé"] {
            let mut a = Ancillary::default();
            a.add_text_international_tagged("Note", language, "", "body", false);
            assert_eq!(
                notices(&a),
                [MetadataNotice::ItxtLanguageTag],
                "language {language:?}"
            );
            assert!(a.validate().is_ok(), "reported, not refused");
            // keyword, NUL, flag, method, *empty* language, NUL, empty translated keyword, NUL.
            assert_eq!(
                find_chunk(&post_plte(&a), b"iTXt"),
                Some(b"Note\0\0\0\0\0body".to_vec()),
                "language {language:?}"
            );
        }

        let mut ok = Ancillary::default();
        ok.add_text_international_tagged("Note", "", "", "body", false);
        ok.add_text_international_tagged("Note", "ar-AE-u-nu-latn", "", "body", false);
        assert!(
            notices(&ok).is_empty(),
            "empty and a full BCP 47 tag are fine"
        );
    }

    /// §11.3.3.4 gives the `iTXt` text field UTF-8 and no alternative, so a packet that is not
    /// UTF-8 has no chunk to go in. It is reported rather than quietly discarded: the read side
    /// surfaces a packet as raw bytes, and a caller that handed those bytes back is entitled to
    /// learn they did not come out the other side. It does not refuse, because the rest of the
    /// file — pixels, colour profile — is fine.
    ///
    /// Kills the `Err` arm of [`Ancillary::add_xmp`] — with it gone the packet vanishes silently
    /// — and its `emit` flag, without which the packet's *keyword* is written with no packet.
    #[test]
    fn a_non_utf8_xmp_packet_is_reported_not_written() {
        let mut a = Ancillary::default();
        a.add_xmp(b"<x:xmpmeta \xFF\xFE/>", "", "", false);
        assert_eq!(notices(&a), [MetadataNotice::XmpNotUtf8]);
        assert!(a.validate().is_ok(), "reported, not refused");
        assert_eq!(find_chunk(&post_plte(&a), b"iTXt"), None);

        let mut valid = Ancillary::default();
        valid.add_xmp(b"<x:xmpmeta/>", "", "", false);
        assert!(notices(&valid).is_empty(), "a UTF-8 packet is carried");
        assert!(find_chunk(&post_plte(&valid), b"iTXt").is_some());
    }

    /// A PNG carries one XMP packet, and §11.3.3.1 Table 21 reserves one keyword for it, so the
    /// encoder's packet is a single-value payload: setting it again replaces it. Appending would
    /// write two `iTXt` chunks under that keyword, and this crate's reader keeps the first — so
    /// the packet set *last* would be the one silently discarded.
    ///
    /// Kills the `retain` in [`Ancillary::add_xmp`]. Asserted on the written chunk rather than on
    /// the entry list because it is the chunk count a reader sees.
    #[test]
    fn setting_an_xmp_packet_twice_writes_one_chunk() {
        let mut a = Ancillary::default();
        a.add_xmp(b"<x:xmpmeta id='first'/>", "", "", false);
        a.add_xmp(b"<x:xmpmeta id='second'/>", "", "", false);
        let out = post_plte(&a);
        assert_eq!(
            find_chunk(&out, b"iTXt"),
            Some(b"XML:com.adobe.xmp\0\0\0\0\0<x:xmpmeta id='second'/>".to_vec())
        );
        assert_eq!(
            out.windows(4).filter(|w| *w == b"iTXt").count(),
            1,
            "one chunk, not two"
        );
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
        a.add_text_latin1("Auth\0or", "body");
        let message = refusal(&a);
        assert!(message.contains("text annotation 1"), "{message}");
        assert!(message.contains(r#""Auth\0or""#), "{message}");
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
