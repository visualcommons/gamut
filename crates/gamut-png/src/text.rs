//! The writer's text-field rules: what §11.3.3 (and §11.3.2.3, which gives an `iCCP` profile
//! name the keyword's rule word for word) says about one keyword, name or text string, resolved
//! into the bytes a chunk carries, the [`MetadataNotice`]s it earns, or the fault that refuses the
//! encode — and the framing of the `tEXt`/`zTXt`/`iTXt` chunk itself ([`write_text`]).
//!
//! Split from `ancillary` (issue #1094): these rules change when the spec's text clauses are
//! read differently, never with how the accumulator orders and emits its chunks. The accumulator
//! holds [`TextEntry`] values and asks this module to build and write them.

use gamut_deflate::{DeflateEncoder, Level};

#[cfg(doc)]
use crate::ancillary::Ancillary;
use crate::carry::MetadataNotice;
use crate::chunk;
use crate::decoded::XMP_KEYWORD;

/// How a text chunk is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextKind {
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
pub(crate) struct TextEntry {
    /// The keyword, Latin-1 (§11.3.3.1). Empty when the keyword had no Latin-1 encoding at all,
    /// which is also when [`emit`](Self::emit) is clear.
    pub(crate) keyword: Vec<u8>,
    /// The text: Latin-1 for `tEXt`/`zTXt`, UTF-8 for `iTXt`.
    pub(crate) text: Vec<u8>,
    /// The `iTXt` language tag (§11.3.3.4, BCP 47); empty for the other kinds and for an
    /// unspecified language.
    language: Vec<u8>,
    /// The `iTXt` translated keyword (UTF-8, §11.3.3.4); empty for the other kinds.
    translated: Vec<u8>,
    kind: TextKind,
    /// Whether this entry came from [`Ancillary::begin_carry`] rather than a direct setter, so a
    /// second carry can replace exactly what the first contributed.
    pub(crate) carried: bool,
    /// Whether the entry is written at all. A cleared flag keeps the entry in the list purely to
    /// carry its [`notices`](Self::notices) — a payload dropped in silence is the defect this
    /// module exists to remove.
    pub(crate) emit: bool,
    /// What §11.3.3 says about this annotation that the caller has to hear.
    ///
    /// Which things those are is [`MetadataNotice`]'s own list of variants, deliberately not
    /// copied here: a copy written out once is what goes stale when a variant is added. An entry
    /// records every notice its fields earn — some meaning the annotation was left behind, some
    /// that it was written with a deviation on it — and
    /// [`MetadataNotice::carried`](crate::MetadataNotice::carried) is which of the two a variant
    /// means. Whether *this* entry reached the output is [`emit`](Self::emit), which
    /// [`Ancillary::text_notices`] takes as the authority when the two disagree. Surfaced by
    /// [`PngEncoder::metadata_notices`](crate::PngEncoder::metadata_notices).
    pub(crate) notices: Vec<MetadataNotice>,
    /// Why this annotation must not be written *at all*, if it must not — a null in the keyword
    /// or in the `iTXt` translated keyword, the two fields a null separator ends, and nothing
    /// else. It refuses the encode only while [`emit`](Self::emit) is set: an entry that is
    /// dropped anyway writes no chunk to re-frame. Recorded here rather than returned from the setter because the
    /// setters sit behind `#[must_use]` builder methods that have no error channel;
    /// [`Ancillary::validate`] reports it at the encode chokepoint.
    pub(crate) fault: Option<TextFault>,
}

impl TextEntry {
    /// Builds the entry for one text annotation, choosing the chunk that can actually carry it
    /// and recording the clause it breaks if no chunk can. `carried` is whether a metadata carry
    /// is pushing it ([`Ancillary::begin_carry`]).
    ///
    /// The caller's `kind` is a *preference*, not a guarantee: §11.3.3.2 says outright that "text
    /// containing characters outside the repertoire of ISO/IEC 8859-1 should be encoded using the
    /// `iTXt` chunk", so a `tEXt`/`zTXt` request whose text [`text_bytes`] cannot encode is
    /// promoted rather than written as bytes a Latin-1 reader mis-renders. The promotion keeps
    /// the caller's *other* choice, compression, because §11.3.3.4 gives `iTXt` a flag of its own.
    ///
    /// A null is forbidden in both fields (§11.3.3.2, §11.3.3.4), and the two are not the same
    /// kind of forbidden. The **keyword** ends at its first null, so a null there makes the chunk
    /// re-parse as a *different* annotation and nothing can undo it: it becomes a [`TextFault`]
    /// the entry carries to [`Ancillary::validate`], and the encode refuses. The **text string**
    /// is last and length-delimited — see [`KEYWORD_NUL`] for both clauses — so a null there
    /// re-frames nothing, and this crate's own reader hands such a text back intact; refusing it
    /// would make the writer stricter than its own reader over a file the reader accepts. It is
    /// not written either, because readers disagree about what the chunk then holds (libpng
    /// truncates the text at the null), so the annotation is dropped and reported as
    /// [`MetadataNotice::TextStringNull`]. Every *other* way a keyword can fall short of
    /// §11.3.3.1 is a [`MetadataNotice`] too: see [`Keyword`] for where that line falls.
    pub(crate) fn new(keyword: &str, text: &str, kind: TextKind, carried: bool) -> Self {
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
        Self {
            keyword: keyword_bytes,
            text: text_bytes,
            language: Vec::new(),
            translated: Vec::new(),
            kind,
            carried,
            emit,
            notices,
            fault: keyword_nul.then(|| TextFault {
                keyword: keyword.to_string(),
                reason: KEYWORD_NUL,
            }),
        }
    }

    /// Builds one `iTXt` entry with its §11.3.3.4 fields, shared by the tagged text setter and
    /// the XMP packet.
    ///
    /// A language tag outside §11.3.3.4's ASCII shape is **dropped, not refused**: written as
    /// UTF-8 into a field a reader takes as Latin-1 it would not survive the trip, but the
    /// annotation itself would, and an unspecified language is what §11.3.3.4 already means by
    /// an empty tag. A null in the translated keyword is a different thing — it re-frames every
    /// field after it — so it refuses, like every other null.
    pub(crate) fn itxt(
        keyword: &str,
        language: &str,
        translated: &str,
        text: &str,
        compressed: bool,
        carried: bool,
    ) -> Self {
        let kind = if compressed {
            TextKind::InternationalCompressed
        } else {
            TextKind::International
        };
        let mut entry = Self::new(keyword, text, kind, carried);
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

    /// Whether a reader will take this entry as the XMP packet: an `iTXt` under the keyword
    /// §11.3.3.1 Table 21 reserves for one — the writer's half of
    /// [`TextChunk::is_xmp_packet`](crate::TextChunk), decided on the kind actually written, so
    /// a Latin-1 request promoted to `iTXt` counts too.
    pub(crate) fn is_xmp_packet(&self) -> bool {
        self.keyword == XMP_KEYWORD.as_bytes()
            && matches!(
                self.kind,
                TextKind::International | TextKind::InternationalCompressed
            )
    }
}

/// Why one accumulated text annotation cannot be written, and which annotation it was.
#[derive(Debug, Clone)]
pub(crate) struct TextFault {
    /// The keyword exactly as the caller gave it, for the refusal message — including a keyword
    /// that is itself the fault.
    pub(crate) keyword: String,
    /// The clause the annotation breaks, phrased for the caller.
    pub(crate) reason: &'static str,
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

/// What the rule §11.3.3.1 gives a text keyword and §11.3.2.3 gives an `iCCP` profile name — word
/// for word the same rule — says about one name, before either field decides what to do with it.
///
/// §11.3.2.3: "Profile name — 1-79 bytes (character string) … Profile names shall contain only
/// printable Latin-1 characters and spaces (only code points 0x20-7E and 0xA1-FF are allowed).
/// Leading, trailing, and consecutive spaces are not permitted." Same repertoire, same length,
/// same spacing as the keyword; and both fields end at their null separator.
enum Name {
    /// The name holds a null, the separator that ends it.
    Null,
    /// A character Latin-1 cannot encode.
    NotLatin1,
    /// Empty, or longer than 79 Latin-1 bytes.
    Length,
    /// Storable: these Latin-1 bytes, with the advisory rule they break, if any.
    Write(Vec<u8>, Option<Shape>),
}

/// Which advisory shape rule a storable [`Name`] breaks.
enum Shape {
    /// A character outside "code points 0x20-7E and 0xA1-FF".
    Repertoire,
    /// A leading, trailing or consecutive space.
    Spacing,
}

/// Resolves `name` against the rule §11.3.3.1 and §11.3.2.3 share.
///
/// Latin-1 representability is settled before the length so that the bound counts *stored*
/// bytes: every character that passes is one Latin-1 byte, which a UTF-8 `str::len` is not.
fn name_verdict(name: &str) -> Name {
    if name.contains('\0') {
        return Name::Null;
    }
    let Some(bytes) = name.chars().map(latin1_byte).collect::<Option<Vec<u8>>>() else {
        return Name::NotLatin1;
    };
    if bytes.is_empty() || bytes.len() > 79 {
        return Name::Length;
    }
    if !name.chars().all(printable_latin1) {
        return Name::Write(bytes, Some(Shape::Repertoire));
    }
    let spacing = name.starts_with(' ') || name.ends_with(' ') || name.contains("  ");
    Name::Write(bytes, spacing.then_some(Shape::Spacing))
}

/// Resolves `keyword` against §11.3.3.1.
fn keyword_verdict(keyword: &str) -> Keyword {
    match name_verdict(keyword) {
        Name::Null => Keyword::Refuse,
        Name::NotLatin1 => Keyword::Drop(MetadataNotice::TextKeywordNotLatin1),
        Name::Length => Keyword::Drop(MetadataNotice::TextKeywordLength),
        Name::Write(bytes, shape) => Keyword::Write(
            bytes,
            shape.map(|shape| match shape {
                Shape::Repertoire => MetadataNotice::TextKeywordRepertoire,
                Shape::Spacing => MetadataNotice::TextKeywordSpacing,
            }),
        ),
    }
}

/// Resolves an `iCCP` profile name against §11.3.2.3: the Latin-1 bytes to write with the notice
/// they carry, or the notice that explains why the chunk is not written.
///
/// The outcomes follow [`Keyword`]'s consequence rule, with one difference. A name outside Latin-1
/// or outside 1–79 bytes is one this crate's reader refuses (`split_keyword`), so writing it would
/// lose the profile between a write and a read with nothing said: **dropped**, and reported. A null
/// is dropped too rather than refusing the encode as a keyword's does: the name ends at it, so
/// the byte after it is read as the compression method and the reader refuses the chunk — the
/// profile is lost, not re-read as a different one, which is the same consequence as the two
/// above. A repertoire or spacing deviation round-trips through this crate's reader byte for byte,
/// so it is **written verbatim** and the non-conformance (§15.3.1) is reported.
pub(crate) fn profile_name_verdict(name: &str) -> core::result::Result<IccpName, MetadataNotice> {
    match name_verdict(name) {
        Name::Null => Err(MetadataNotice::IccpNameNull),
        Name::NotLatin1 => Err(MetadataNotice::IccpNameNotLatin1),
        Name::Length => Err(MetadataNotice::IccpNameLength),
        Name::Write(bytes, shape) => Ok((
            bytes,
            shape.map(|shape| match shape {
                Shape::Repertoire => MetadataNotice::IccpNameRepertoire,
                Shape::Spacing => MetadataNotice::IccpNameSpacing,
            }),
        )),
    }
}

/// A writable `iCCP` profile name: its Latin-1 bytes, and the advisory notice it carries.
pub(crate) type IccpName = (Vec<u8>, Option<MetadataNotice>);

/// The `iCCP` profile name written when none is set: 1–79 printable Latin-1 characters with no
/// leading, trailing or consecutive space, as §11.3.2.3 requires, so a profile set without a name
/// draws no notice.
pub(crate) const DEFAULT_ICC_PROFILE_NAME: &str = "ICC profile";

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
/// language tag defined by \[BCP47\]", whose subtags are ASCII letters and digits joined by
/// hyphens. This checks the character set, not full BCP 47 well-formedness.
fn well_formed_language(language: &str) -> bool {
    language
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Serialises one text annotation. `entry.text` is already in the chunk's character set — Latin-1
/// for `tEXt`/`zTXt` (§11.3.3.2, §11.3.3.3), UTF-8 for `iTXt` (§11.3.3.4) — because
/// [`TextEntry::new`] converted it when the caller set it, so this function only frames
/// the bytes.
pub(crate) fn write_text(out: &mut Vec<u8>, entry: &TextEntry, effort: u8) {
    let compress = |payload: &[u8], data: &mut Vec<u8>| {
        DeflateEncoder::new()
            .with_level(Level::Best)
            .with_deflate_passes(effort)
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
