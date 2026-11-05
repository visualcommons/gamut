//! Standard ancillary chunks (PNG spec §11.3): colour-space, physical, timing, and text metadata.
//!
//! These are optional. The encoder accumulates whatever the caller sets and emits the chunks in the
//! order PNG requires (Table 7): colour-space chunks before `PLTE`, the rest before `IDAT`.
//!
//! One chunk here is not PNG's own: the C2PA manifest store, `caBX` (C2PA 2.4 §A.3.2). It is
//! emitted **last** of everything before `IDAT`, so that its offset depends only on the chunks
//! that precede it and every byte after it is `IDAT` or `IEND` — which is what lets a reserved
//! store be filled in place ([`crate::fill_c2pa`]) without moving a byte outside the chunk.
//! C2PA §A.3.2 asks only that it precede `IDAT`.
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
//! Two concerns live beside this accumulator rather than in it. The text-field rules — what
//! §11.3.3 says about a keyword, a profile name or a text string, and the framing of the text
//! chunks — are [`crate::text`]'s. The conversion of the colour-type-shaped `bKGD` and `sBIT`
//! payloads to the header actually written is [`crate::written`]'s.

use gamut_core::{Error, Result};
use gamut_deflate::{DeflateEncoder, Level};

use crate::carry::MetadataNotice;
use crate::chunk;
use crate::decoded::{Background, Cicp, ModificationTime, PhysicalDimensions, XMP_KEYWORD};
use crate::text::{
    DEFAULT_ICC_PROFILE_NAME, IccpName, TextEntry, TextKind, profile_name_verdict, write_text,
};
use crate::written::{WrittenHeader, bkgd_for, sbit_for};

/// The rendering intent for an `sRGB` chunk (PNG spec §11.3.2.5).
///
/// `#[repr(u8)]` with the chunk's intent codes (§11.3.2.5 Table 16) as its discriminants, which
/// are permanent: the value crosses the C ABI as that plain integer. Closed, because the table
/// defines exactly these four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SrgbIntent {
    /// Perceptual (intent code 0).
    Perceptual = 0,
    /// Relative colorimetric (intent code 1).
    RelativeColorimetric = 1,
    /// Saturation (intent code 2).
    Saturation = 2,
    /// Absolute colorimetric (intent code 3).
    AbsoluteColorimetric = 3,
}

impl SrgbIntent {
    /// The chunk's intent code: the discriminant.
    const fn code(self) -> u8 {
        self as u8
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
///
/// `#[repr(u8)]` with the chunk's unit specifiers (§11.3.4.3 Table 24) as its discriminants,
/// which are permanent: the value crosses the C ABI as that plain integer. Closed, because the
/// table defines exactly these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PhysicalUnit {
    /// Unit is unknown; the values give only an aspect ratio (unit code 0).
    Unknown = 0,
    /// Pixels per metre (unit code 1).
    Meter = 1,
}

impl PhysicalUnit {
    /// The chunk's unit specifier: the discriminant.
    const fn code(self) -> u8 {
        self as u8
    }

    /// The unit for a `pHYs` unit specifier, or `None` for a value §11.3.4.3 Table 24 does not
    /// define.
    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(PhysicalUnit::Unknown),
            1 => Some(PhysicalUnit::Meter),
            _ => None,
        }
    }
}

/// Whether a `tIME`'s month, day, hour, minute and second each lie within the range §11.3.5.1's
/// Table 26 gives it (1–12, 1–31, 0–23, 0–59, 0–60; the year is any two-byte value). The one rule
/// both directions apply: the reader skips a chunk outside it as malformed (§13.1), and the writer
/// drops one with [`MetadataNotice::TimeOutOfRange`], so it never writes a `tIME` its own reader
/// would skip.
pub(crate) fn time_in_range(month: u8, day: u8, hour: u8, minute: u8, second: u8) -> bool {
    (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 60
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
    /// coefficients byte is not carried because §11.3.2.6 fixes it at 0 for PNG. Set through
    /// [`Self::set_cicp`], which leaves it empty for a non-RGB matrix.
    pub cicp: Option<(u8, u8, bool)>,
    /// Why the code points last given to [`Self::set_cicp`] were dropped, if they were. A
    /// single-value slot like [`cicp`](Self::cicp) itself, so setting code points again replaces
    /// it.
    cicp_notice: Option<MetadataNotice>,
    /// sBIT: significant bits per channel (1–4 values, matching the colour type).
    pub sbit: Option<Vec<u8>>,
    /// bKGD: background colour, pre-serialised to its colour-type-specific bytes.
    pub bkgd: Option<Vec<u8>>,
    /// pHYs: (pixels-per-unit X, Y, unit code).
    pub phys: Option<(u32, u32, u8)>,
    /// tIME: year, month, day, hour, minute, second. Set through [`Self::set_time`], which
    /// leaves it empty for a time outside Table 26's ranges.
    pub time: Option<[u8; 7]>,
    /// Why the time last given to [`Self::set_time`] was dropped, if it was. A single-value slot
    /// like [`time`](Self::time) itself, so setting a time again replaces it.
    time_notice: Option<MetadataNotice>,
    /// iCCP: the raw ICC profile bytes, compressed at emit time. Set through
    /// [`Self::set_icc_profile`]; [`Self::iccp`] is what is written.
    pub icc_profile: Option<Vec<u8>>,
    /// The `iCCP` profile name last given to [`Self::set_icc_profile_name`], or `None` for
    /// [`DEFAULT_ICC_PROFILE_NAME`]. Held as given; §11.3.2.3's verdict on it is taken where the
    /// chunk is written or reported ([`Self::iccp`], [`Self::iccp_notice`]), so setting the name
    /// or the profile again replaces both the slot and its notice.
    icc_name: Option<String>,
    /// eXIf: raw EXIF/TIFF bytes (the chunk payload starts with the TIFF byte-order marker).
    pub exif: Option<Vec<u8>>,
    /// caBX: the C2PA manifest store, raw and uncompressed (C2PA 2.4 §A.3.2) — or a run of zero
    /// bytes reserving its place. Emitted last, immediately before the first `IDAT`.
    pub c2pa: Option<C2paStore>,
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

    pub(crate) fn set_physical(&mut self, phys: PhysicalDimensions) {
        self.phys = Some((phys.x_ppu, phys.y_ppu, phys.unit.code()));
    }

    /// Sets the `cICP` code points, or — for matrix coefficients other than 0, which §11.3.2.6
    /// forbids in PNG ("Matrix Coefficients shall be set to 0") — leaves the slot empty and
    /// records [`MetadataNotice::NonRgbCicp`] in its place.
    pub(crate) fn set_cicp(&mut self, cicp: Cicp) {
        let rgb = cicp.matrix_coefficients == 0;
        self.cicp = rgb.then_some((
            cicp.color_primaries,
            cicp.transfer_function,
            cicp.full_range,
        ));
        self.cicp_notice = (!rgb).then_some(MetadataNotice::NonRgbCicp);
    }

    /// Why the code points last set were dropped, if they were.
    pub(crate) fn cicp_notice(&self) -> Option<MetadataNotice> {
        self.cicp_notice
    }

    /// Sets the `bKGD` background in the form its variant names, as the colour-type-specific
    /// bytes §11.3.4.1 Table 22 gives it: a 2-byte grey, three 2-byte samples, or a 1-byte index.
    pub(crate) fn set_background(&mut self, background: Background) {
        self.bkgd = Some(match background {
            Background::Gray(gray) => gray.to_be_bytes().to_vec(),
            Background::Rgb(red, green, blue) => [red, green, blue]
                .iter()
                .flat_map(|sample| sample.to_be_bytes())
                .collect(),
            Background::Index(index) => vec![index],
        });
    }

    /// Sets the `tIME` time, or — for a field outside its Table 26 range, which this crate's
    /// reader skips as malformed — leaves the slot empty and records
    /// [`MetadataNotice::TimeOutOfRange`] in its place.
    pub(crate) fn set_time(&mut self, time: ModificationTime) {
        let ModificationTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
        } = time;
        let [yh, yl] = year.to_be_bytes();
        let in_range = time_in_range(month, day, hour, minute, second);
        self.time = in_range.then_some([yh, yl, month, day, hour, minute, second]);
        self.time_notice = (!in_range).then_some(MetadataNotice::TimeOutOfRange);
    }

    /// Why the time last set was dropped, if it was.
    pub(crate) fn time_notice(&self) -> Option<MetadataNotice> {
        self.time_notice
    }

    /// Sets the raw `iCCP` profile, written under the name [`Self::set_icc_profile_name`] last
    /// gave (or [`DEFAULT_ICC_PROFILE_NAME`]).
    pub(crate) fn set_icc_profile(&mut self, profile: &[u8]) {
        self.icc_profile = Some(profile.to_vec());
    }

    /// Sets the name the `iCCP` profile is written under, replacing the previous one.
    pub(crate) fn set_icc_profile_name(&mut self, name: &str) {
        self.icc_name = Some(name.to_owned());
    }

    /// §11.3.2.3's verdict on the profile name, routed through [`profile_name_verdict`], or `None`
    /// when no profile is set and so no chunk is in question.
    fn iccp_verdict(&self) -> Option<core::result::Result<IccpName, MetadataNotice>> {
        self.icc_profile.as_ref()?;
        let name = self.icc_name.as_deref().unwrap_or(DEFAULT_ICC_PROFILE_NAME);
        Some(profile_name_verdict(name))
    }

    /// The `iCCP` chunk to write: the profile name as its Latin-1 bytes and the raw profile, or
    /// `None` when no profile is set or its name is one no chunk this crate's reader accepts can
    /// hold.
    pub(crate) fn iccp(&self) -> Option<(Vec<u8>, &[u8])> {
        let (name, _) = self.iccp_verdict()?.ok()?;
        Some((name, self.icc_profile.as_deref()?))
    }

    /// What §11.3.2.3 says about the profile name in the `iCCP` slot, if anything: why the
    /// profile is dropped, or the deviation it is written with.
    pub(crate) fn iccp_notice(&self) -> Option<MetadataNotice> {
        match self.iccp_verdict()? {
            Ok((_, notice)) => notice,
            Err(notice) => Some(notice),
        }
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
        let entry = TextEntry::itxt(
            keyword,
            language,
            translated,
            text,
            compressed,
            self.carrying,
        );
        self.texts.push(entry);
    }

    /// Sets the XMP packet: the `iTXt` §11.3.3.1 Table 21 reserves for it, in the framing Table
    /// 21 recommends — "Compression Flag set to 0, and both Language Tag and Translated Keyword
    /// set to the null string". A packet carried out of a file keeps its own framing because it
    /// arrives as an ordinary annotation through
    /// [`add_text_international_tagged`](Self::add_text_international_tagged); this is the
    /// entry point with no framing to keep.
    ///
    /// Replaces every packet already accumulated ([`drop_xmp`](Self::drop_xmp)) rather than
    /// adding a second: the caller is naming *the* packet, and two `iTXt` chunks under the
    /// reserved keyword leave a reader to pick — this crate's own reader takes the first, so the
    /// packet set *last* would be the one ignored.
    pub(crate) fn add_xmp(&mut self, packet: &str) {
        self.drop_xmp();
        let entry = TextEntry::itxt(XMP_KEYWORD, "", "", packet, false, self.carrying);
        self.texts.push(entry);
    }

    /// Removes every accumulated entry a reader would take as the XMP packet
    /// ([`TextEntry::is_xmp_packet`]), so a new one replaces rather than joins it.
    pub(crate) fn drop_xmp(&mut self) {
        self.texts.retain(|entry| !entry.is_xmp_packet());
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
    /// This is what makes [`PngEncoder::with_png_metadata`](crate::PngEncoder::with_png_metadata)
    /// idempotent for text. The single-value slots — `gamma`, `iccp`, `srgb`, … — are idempotent
    /// already because a second write overwrites the first; the text list is the one place where
    /// "set it again" would otherwise mean "append it again", duplicating every annotation.
    pub(crate) fn begin_carry(&mut self) {
        self.texts.retain(|entry| !entry.carried);
        self.carrying = true;
    }

    /// Ends the carry started by [`begin_carry`](Self::begin_carry), so later direct setters push
    /// entries a subsequent carry will not remove.
    ///
    /// A carry that brought an XMP packet displaces any packet set directly before it, so the
    /// encoder writes the packet it was last handed, as [`add_xmp`](Self::add_xmp) does the
    /// other way round; a carry that brought none leaves a directly-set packet alone. Whether
    /// the carry brought one is decided on the **written** entries ([`TextEntry::is_xmp_packet`]),
    /// not on the source's chunk kinds: a carried `tEXt` under the reserved keyword whose text
    /// has no Latin-1 encoding is promoted to `iTXt` and becomes a packet a reader takes, so it
    /// must displace like any other, or the file would hold two.
    pub(crate) fn end_carry(&mut self) {
        if self
            .texts
            .iter()
            .any(|entry| entry.carried && entry.is_xmp_packet())
        {
            self.texts
                .retain(|entry| entry.carried || !entry.is_xmp_packet());
        }
        self.carrying = false;
    }

    fn push_text(&mut self, keyword: &str, text: &str, kind: TextKind) {
        let entry = TextEntry::new(keyword, text, kind, self.carrying);
        self.texts.push(entry);
    }

    /// Refuses an accumulation the spec forbids, before any byte is emitted.
    ///
    /// **Only a null in a keyword of an annotation that is written gets here.** An entry that is
    /// not emitted anyway — dropped for a null in its text string or a keyword outside Latin-1 —
    /// writes no chunk for the null to re-frame, so its fault refuses nothing; the notice that
    /// explains the drop is what the caller hears. A null in a text chunk's keyword or in an
    /// `iTXt` translated keyword (§11.3.3.2, §11.3.3.4) sits in a field a null separator *ends*,
    /// so a chunk carrying one re-parses as a *different* annotation: the file would mean
    /// something other than what the caller supplied, and no notice can undo that. Everything
    /// else §11.3.3 asks of a text chunk — the keyword's repertoire, length and spacing, a null in
    /// the length-delimited text string, the `iTXt` language tag's shape — is reported through
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
            if let (true, Some(fault)) = (entry.emit, &entry.fault) {
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
        if let Some((name, profile)) = self.iccp() {
            // The name is already its Latin-1 bytes, 1–79 of them and null-free: `iccp` lets
            // nothing else through (§11.3.2.3).
            let mut data = name;
            data.push(0); // null separator
            data.push(0); // compression method: 0 = zlib/deflate
            DeflateEncoder::new()
                .with_level(Level::Best)
                .with_deflate_passes(effort)
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
        match &self.c2pa {
            Some(C2paStore::Store(store)) => chunk::write_chunk(out, chunk::CABX, store),
            Some(C2paStore::Reserved(len)) => chunk::write_chunk(out, chunk::CABX, &vec![0; *len]),
            None => {}
        }
    }
}

/// The `caBX` payload the caller asked for.
///
/// A reservation is held as its length, not as the zero bytes it stands for, so nothing is
/// allocated for it until the encode writes it: [`PngEncoder::with_c2pa_reserved`](crate::PngEncoder::with_c2pa_reserved)
/// checks the length against PNG §5.3's 2^31 − 1 bound, and the zero run is built only by the
/// write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum C2paStore {
    /// A finished store, written verbatim.
    Store(Vec<u8>),
    /// A placeholder of this many zero bytes.
    Reserved(usize),
}

impl C2paStore {
    /// The payload length the chunk will carry.
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Store(store) => store.len(),
            Self::Reserved(len) => *len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorType;
    use crate::written::tests::{header, indexed};

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
        unitless.set_physical(PhysicalDimensions::new(300, 300, PhysicalUnit::Unknown));
        assert_eq!(unitless.phys, Some((300, 300, 0)));

        let mut metric = Ancillary::default();
        metric.set_physical(PhysicalDimensions::new(2835, 2835, PhysicalUnit::Meter));
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
        a.write_pre_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
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
        a.set_physical(PhysicalDimensions::new(2835, 2835, PhysicalUnit::Meter));
        a.set_time(ModificationTime::new(2026, 6, 13, 1, 2, 3));
        a.add_text_latin1("Title", "hi");
        let mut out = vec![0u8; 8];
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
        let phys = find_chunk(&out, b"pHYs").unwrap();
        assert_eq!(&phys[0..4], 2835u32.to_be_bytes());
        assert_eq!(phys[8], 1); // metre
        assert_eq!(
            find_chunk(&out, b"tIME").unwrap(),
            vec![7, 234, 6, 13, 1, 2, 3]
        ); // 2026 = 0x07EA
        assert_eq!(find_chunk(&out, b"tEXt").unwrap(), b"Title\0hi".to_vec());
    }

    /// A time outside Table 26's ranges — one this crate's reader skips — is not written, and
    /// says why; setting an in-range time afterwards writes it and clears the notice. The bounds
    /// themselves are [`time_in_range`]'s, pinned field by field on the read side.
    ///
    /// Kills `set_time` writing the slot or the notice regardless of the range check.
    #[test]
    fn an_out_of_range_time_is_dropped_with_a_notice() {
        let mut a = Ancillary::default();
        a.set_time(ModificationTime::new(2026, 13, 1, 0, 0, 0));
        assert_eq!(a.time, None, "month 13 is not written");
        assert_eq!(a.time_notice(), Some(MetadataNotice::TimeOutOfRange));
        let mut out = vec![0u8; 8];
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
        assert_eq!(find_chunk(&out, b"tIME"), None);
        assert!(!MetadataNotice::TimeOutOfRange.carried());

        a.set_time(ModificationTime::new(2026, 12, 31, 23, 59, 60));
        assert_eq!(a.time, Some([7, 234, 12, 31, 23, 59, 60]));
        assert_eq!(
            a.time_notice(),
            None,
            "the in-range time replaces the notice"
        );
    }

    /// The manifest store is the last chunk the pre-IDAT pass writes, after every text entry
    /// added before or after it was set, and it is written raw: no keyword, no compression byte.
    #[test]
    fn the_c2pa_store_is_written_raw_and_last_before_idat() {
        let mut a = Ancillary::default();
        a.add_text_latin1("Before", "set first");
        a.c2pa = Some(C2paStore::Store(b"\0\0\0\x1fjumb".to_vec()));
        a.add_text_compressed("After", "set later");
        a.set_time(ModificationTime::new(2026, 9, 6, 0, 0, 0));
        let mut out = vec![0u8; 8];
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);

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
        Ancillary::default().write_post_plte(
            &mut none,
            DeflateEncoder::DEFAULT_DEFLATE_PASSES,
            RGB8,
        );
        assert_eq!(find_chunk(&none, b"caBX"), None);
    }

    /// §11.3.2.6 admits matrix coefficients 0 only: [`Ancillary::set_cicp`] keeps RGB code points
    /// with no notice, drops any other matrix with [`MetadataNotice::NonRgbCicp`], and a later
    /// call replaces both the slot and its notice.
    #[test]
    fn set_cicp_keeps_rgb_code_points_and_drops_any_other_matrix_with_a_notice() {
        let mut a = Ancillary::default();
        a.set_cicp(Cicp::new(9, 16, true));
        assert_eq!((a.cicp, a.cicp_notice()), (Some((9, 16, true)), None));

        let mut non_rgb = Cicp::new(1, 13, false);
        non_rgb.matrix_coefficients = 1;
        a.set_cicp(non_rgb);
        assert_eq!(
            (a.cicp, a.cicp_notice()),
            (None, Some(MetadataNotice::NonRgbCicp))
        );

        a.set_cicp(Cicp::new(1, 13, false));
        assert_eq!((a.cicp, a.cicp_notice()), (Some((1, 13, false)), None));
    }

    /// [`Ancillary::set_background`] stores each [`Background`] form as the bytes §11.3.4.1
    /// Table 22 gives it: two for a grey, six for an RGB triple, one for an index — big-endian.
    #[test]
    fn set_background_serialises_each_form_as_table_22_gives_it() {
        let mut a = Ancillary::default();
        a.set_background(Background::Gray(0x1234));
        assert_eq!(a.bkgd, Some(vec![0x12, 0x34]));
        a.set_background(Background::Rgb(0x0102, 0x0304, 0x0506));
        assert_eq!(a.bkgd, Some(vec![1, 2, 3, 4, 5, 6]));
        a.set_background(Background::Index(7));
        assert_eq!(a.bkgd, Some(vec![7]));
    }

    /// The `iCCP` payload [`Ancillary::set_icc_profile`] leads to, if any.
    fn iccp_chunk(name: &str) -> (Option<Vec<u8>>, Option<MetadataNotice>) {
        let mut a = Ancillary::default();
        a.set_icc_profile_name(name);
        a.set_icc_profile(b"profile");
        let mut pre = vec![0u8; 8];
        a.write_pre_plte(&mut pre, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
        (find_chunk(&pre, b"iCCP"), a.iccp_notice())
    }

    /// The bytes an `iCCP` payload opens with for `name`: its Latin-1 bytes, the null separator,
    /// and compression method 0.
    fn iccp_head(name: &[u8]) -> Vec<u8> {
        let mut head = name.to_vec();
        head.extend_from_slice(b"\0\0");
        head
    }

    /// §11.3.2.3 gives the profile name printable Latin-1, and the reader decodes it one byte per
    /// character, so a name written as UTF-8 comes back as a different name — and a 79-byte one
    /// holding a character above U+007F comes back not at all. Kills the Latin-1 conversion in
    /// [`Ancillary::set_icc_profile`] and the `Ok` arm of [`profile_name_verdict`].
    #[test]
    fn an_iccp_profile_name_is_written_as_latin1() {
        let (chunk, notice) = iccp_chunk("café");
        assert_eq!(notice, None);
        assert!(chunk.unwrap().starts_with(&iccp_head(b"caf\xE9")));
    }

    /// §11.3.2.3: "Profile name — 1-79 bytes", in Latin-1, ended by its null separator. A name
    /// outside that leaves this crate's reader refusing the chunk, so the profile is dropped here
    /// and reported instead of being lost on the next read.
    ///
    /// Kills each `Err` arm of [`profile_name_verdict`] and the `Err` arm of
    /// [`Ancillary::set_icc_profile`] that keeps the slot empty.
    #[test]
    fn an_iccp_profile_name_no_reader_accepts_drops_the_profile_with_a_notice() {
        for (name, expected) in [
            ("œrofile", MetadataNotice::IccpNameNotLatin1),
            ("", MetadataNotice::IccpNameLength),
            (&"p".repeat(80), MetadataNotice::IccpNameLength),
            ("pro\0file", MetadataNotice::IccpNameNull),
        ] {
            let (chunk, notice) = iccp_chunk(name);
            assert_eq!(notice, Some(expected), "name {name:?}");
            assert_eq!(chunk, None, "name {name:?}");
        }
        let (chunk, notice) = iccp_chunk(&"p".repeat(79));
        assert_eq!(notice, None, "79 bytes is inside");
        assert!(chunk.is_some());
    }

    /// §11.3.2.3's repertoire ("only code points 0x20-7E and 0xA1-FF are allowed") and spacing
    /// ("Leading, trailing, and consecutive spaces are not permitted") are names this crate's
    /// reader returns unchanged, so the profile is written with its name verbatim and the
    /// deviation reported, as a keyword's is.
    ///
    /// Kills the two arms of [`profile_name_verdict`]'s shape mapping, and asserts each of the
    /// two alternatives [`MetadataNotice::carried`] gained for them separately.
    #[test]
    fn an_iccp_profile_name_off_the_repertoire_or_spacing_is_written_with_a_notice() {
        for (name, expected) in [
            ("pro\u{A0}file", MetadataNotice::IccpNameRepertoire),
            (" profile", MetadataNotice::IccpNameSpacing),
        ] {
            let (chunk, notice) = iccp_chunk(name);
            assert_eq!(notice, Some(expected), "name {name:?}");
            assert!(expected.carried(), "{expected:?} reports a written profile");
            let bytes: Vec<u8> = name.chars().map(|c| c as u8).collect();
            assert!(
                chunk.unwrap().starts_with(&iccp_head(&bytes)),
                "name {name:?}"
            );
        }
    }

    /// The notice belongs to the name and profile last set: a valid name set after a dropped one
    /// clears it, and an invalid one set after a valid profile empties the slot. A name with no
    /// profile draws no notice, since no chunk is in question, and a profile with no name is
    /// written under [`DEFAULT_ICC_PROFILE_NAME`]. Kills a setter that keeps either half of the
    /// previous state.
    #[test]
    fn setting_an_iccp_profile_or_name_again_replaces_its_notice_and_slot() {
        let mut a = Ancillary::default();
        a.set_icc_profile_name("");
        assert_eq!((a.iccp(), a.iccp_notice()), (None, None));
        a.set_icc_profile(b"profile");
        assert_eq!(a.iccp_notice(), Some(MetadataNotice::IccpNameLength));
        a.set_icc_profile_name("p");
        assert_eq!(a.iccp_notice(), None);
        assert_eq!(a.iccp(), Some((b"p".to_vec(), &b"profile"[..])));
        a.set_icc_profile_name("");
        assert_eq!(a.iccp_notice(), Some(MetadataNotice::IccpNameLength));
        assert_eq!(a.iccp(), None);

        let mut unnamed = Ancillary::default();
        unnamed.set_icc_profile(b"profile");
        assert_eq!(unnamed.iccp_notice(), None);
        assert_eq!(
            unnamed.iccp(),
            Some((
                DEFAULT_ICC_PROFILE_NAME.as_bytes().to_vec(),
                &b"profile"[..]
            ))
        );
    }

    #[test]
    fn iccp_and_exif_framing() {
        let mut a = Ancillary {
            icc_profile: Some(b"the quick brown fox".to_vec()),
            exif: Some(vec![0x49, 0x49, 0x2A, 0x00]),
            ..Default::default()
        };
        a.set_icc_profile_name("p");
        let mut pre = vec![0u8; 8];
        a.write_pre_plte(&mut pre, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
        let iccp = find_chunk(&pre, b"iCCP").unwrap();
        assert_eq!(&iccp[..2], b"p\0"); // profile name + null
        assert_eq!(iccp[2], 0); // compression method
        assert_eq!(iccp[3], 0x78); // zlib CMF byte begins the compressed profile

        let mut post = vec![0u8; 8];
        a.write_post_plte(&mut post, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
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
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
        let data = find_chunk(&out, b"zTXt").unwrap();
        assert_eq!(&data[..8], b"Comment\0");
        assert_eq!(data[8], 0); // compression method
        assert_eq!(data[9], 0x78); // the zlib CMF byte begins the compressed text
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
        a.write_pre_plte(&mut pre, DeflateEncoder::DEFAULT_DEFLATE_PASSES, indexed(8));
        assert_eq!(find_chunk(&pre, b"sBIT"), Some(vec![5, 6, 5]));

        let mut post = vec![0u8; 8];
        a.write_post_plte(
            &mut post,
            DeflateEncoder::DEFAULT_DEFLATE_PASSES,
            header(ColorType::Grayscale, 8),
        );
        assert_eq!(find_chunk(&post, b"bKGD"), None);
    }

    /// Encodes `a`'s post-PLTE chunks and returns the buffer, so a claim can read the bytes a
    /// text annotation actually becomes.
    fn post_plte(a: &Ancillary) -> Vec<u8> {
        let mut out = vec![0u8; 8];
        a.write_post_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
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
    /// Kills the `None` arm of [`TextEntry::new`]'s promotion. The keyword stays Latin-1
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
        a.set_icc_profile(&[0u8; 4]);
        assert!(a.validate().is_ok(), "the pair is legal");

        let mut out = vec![0u8; 8];
        a.write_pre_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
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
    /// by one, and the `Drop` arm of [`TextEntry::new`] that keeps the chunk out.
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
    /// Kills the text-null guard in [`TextEntry::new`], in both the Latin-1 and the UTF-8
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
    /// Kills the translated-keyword null check in [`TextEntry::itxt`].
    #[test]
    fn a_null_in_a_translated_keyword_is_refused() {
        let mut a = Ancillary::default();
        a.add_text_international_tagged("Note", "de", "No\0tiz", "body", false);
        assert!(refusal(&a).contains("translated keyword may not contain a null"));
    }

    /// A null fault refuses only an annotation that would be written. One also dropped for a null
    /// in its text string, or for a keyword outside Latin-1, writes no chunk the null could
    /// re-frame, so the encode proceeds and the drop is reported; the same faults on an emitted
    /// entry still refuse (the two tests above).
    ///
    /// Kills the `emit` guard in [`Ancillary::validate`].
    #[test]
    fn a_null_fault_on_an_annotation_never_written_refuses_nothing() {
        let mut keyword = Ancillary::default();
        keyword.add_text_latin1("Auth\0or", "before\0after");
        assert!(keyword.validate().is_ok(), "nothing is written to re-frame");
        assert_eq!(notices(&keyword), [MetadataNotice::TextStringNull]);
        assert_eq!(find_chunk(&post_plte(&keyword), b"tEXt"), None);

        let mut translated = Ancillary::default();
        translated.add_text_international_tagged("\u{153}kw", "de", "No\0tiz", "body", false);
        assert!(
            translated.validate().is_ok(),
            "nothing is written to re-frame"
        );
        assert_eq!(notices(&translated), [MetadataNotice::TextKeywordNotLatin1]);
        assert_eq!(find_chunk(&post_plte(&translated), b"iTXt"), None);
    }

    /// §11.3.3.4: "The language tag is a well-formed language tag defined by \[BCP47\]", whose
    /// subtags are ASCII letters and digits joined by hyphens. Anything else, written as UTF-8
    /// into a field a reader takes as Latin-1, would not survive the trip — so the **tag** goes
    /// and the annotation stays, an empty tag being §11.3.3.4's own way of saying the language is
    /// unspecified.
    ///
    /// Kills the language arm of [`TextEntry::itxt`]; the empty case pins that
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

    /// Setting the packet replaces only what a reader would take as one: an `iTXt` under the
    /// reserved keyword, compressed or not. A `tEXt` that borrows the keyword is not a packet
    /// (Table 21 names `iTXt`), and neither is an `iTXt` under another keyword, so both survive.
    ///
    /// Kills [`TextEntry::is_xmp_packet`]: each operand of its `&&` and each alternative of its
    /// kind pattern.
    #[test]
    fn setting_the_packet_replaces_only_itxt_under_the_reserved_keyword() {
        let mut a = Ancillary::default();
        a.add_text_latin1(XMP_KEYWORD, "latin");
        a.add_text_international_tagged("Other", "", "", "other", false);
        a.add_text_international_tagged(XMP_KEYWORD, "", "", "plain", false);
        a.add_text_international_tagged(XMP_KEYWORD, "", "", "squeezed", true);
        a.add_xmp("<x:xmpmeta/>");
        let texts: Vec<&[u8]> = a.texts.iter().map(|entry| &entry.text[..]).collect();
        assert_eq!(
            texts,
            [&b"latin"[..], b"other", b"<x:xmpmeta/>"],
            "both packets replaced; the tEXt and the other iTXt kept"
        );
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
        a.add_xmp("<x:xmpmeta id='first'/>");
        a.add_xmp("<x:xmpmeta id='second'/>");
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

    /// A carry displaces a directly-set XMP packet when what it **wrote** is a packet: here a
    /// `tEXt` request under the reserved keyword whose text has no Latin-1 encoding, promoted to
    /// `iTXt`. Deciding on the source's kind instead leaves two packets. A carry that wrote no
    /// packet keeps the direct one, and an ordinary direct annotation survives either way.
    ///
    /// Kills [`Ancillary::end_carry`]'s displacement: the guard (each operand of its `&&`) and
    /// the `retain` (each operand of its `||`).
    #[test]
    fn a_carry_displaces_a_direct_packet_only_when_it_writes_one() {
        let packets = |a: &Ancillary| -> Vec<(Vec<u8>, bool)> {
            a.texts
                .iter()
                .filter(|e| e.is_xmp_packet())
                .map(|e| (e.text.clone(), e.carried))
                .collect()
        };

        let mut a = Ancillary::default();
        a.add_text_latin1("Before", "kept");
        a.add_xmp("direct");
        a.begin_carry();
        a.add_text_latin1(XMP_KEYWORD, "promoted \u{2192}");
        a.end_carry();
        assert_eq!(
            packets(&a),
            [("promoted \u{2192}".as_bytes().to_vec(), true)]
        );
        assert!(a.texts.iter().any(|e| e.keyword == b"Before"));

        let mut b = Ancillary::default();
        b.add_xmp("direct");
        b.begin_carry();
        b.add_text_latin1(XMP_KEYWORD, "latin-1 stays tEXt");
        b.end_carry();
        assert_eq!(packets(&b), [(b"direct".to_vec(), false)]);
    }

    /// A carry replaces what an earlier carry contributed instead of appending a second copy, so
    /// `with_png_metadata` is idempotent for text the way the single-value colour slots already are.
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
        full.write_pre_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
        assert_eq!(find_chunk(&out, b"cICP"), Some(vec![9, 16, 0, 1]));

        let narrow = Ancillary {
            cicp: Some((1, 13, false)),
            ..Default::default()
        };
        let mut out = vec![0u8; 8];
        narrow.write_pre_plte(&mut out, DeflateEncoder::DEFAULT_DEFLATE_PASSES, RGB8);
        assert_eq!(find_chunk(&out, b"cICP"), Some(vec![1, 13, 0, 0]));
    }
}
