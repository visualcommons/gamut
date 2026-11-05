//! Carrying a read file's metadata into a re-encode: [`PngEncoder::with_png_metadata`], and the
//! [`MetadataNotice`]s that name what a carry — or a raw setter — could not take faithfully.
//!
//! Split from `encoder` (issue #1094): the carry changes when the read side surfaces a new
//! chunk or the spec's text rules move, never with the encode pipeline. It reaches the
//! encoder's ancillary accumulator only through the same setters a caller uses, plus the
//! accumulator's carry bracket.

use crate::PngEncoder;
use crate::decoded::{PngMetadata, TextChunkKind};

/// Something [`PngEncoder::with_png_metadata`] could not do faithfully with a payload it was given.
///
/// Preservation exists to stop metadata disappearing quietly, so anything a carry cannot take —
/// and anything it takes only by writing bytes the specification does not endorse — is named
/// rather than passed over. Read them back with [`PngEncoder::metadata_notices`] and tell the
/// user; `gamut convert` does. [`carried`](Self::carried) separates the two cases: a payload
/// left behind from one that reached the output with a caveat on it.
///
/// This is deliberately **not** an error channel. The only thing that stops an encode is a null
/// byte in a *keyword* — a field a null separator ends, so the chunk would re-parse as a
/// different annotation; everything here is something a caller has to *know*, not something that
/// should fail a conversion whose pixels are fine.
///
/// `#[repr(u8)]` with explicit discriminants, which are permanent and append-only: the value
/// crosses the C ABI as a plain integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum MetadataNotice {
    /// A `cICP` whose matrix coefficients are not 0, left behind. §11.3.2.6 requires 0 for PNG —
    /// "RGB is currently the only supported color model in PNG, and as such Matrix Coefficients
    /// shall be set to 0" — so the source chunk is not conforming and copying it forward would
    /// reproduce the defect in a file this encoder signed off on.
    NonRgbCicp = 0,
    /// The C2PA manifest store (`caBX`), left behind. A store is signed over the exact bytes of
    /// the file it was made for, which is why C2PA 2.4 §A.3.2 marks the chunk unsafe to copy:
    /// carried into a re-encode it is invalid by construction, and a validator reports a
    /// *tampered* file rather than an unsigned one. Re-sign the output and set it with
    /// [`with_c2pa`](PngEncoder::with_c2pa).
    C2paManifestStore = 1,
    /// A text annotation left behind because its keyword holds a character Latin-1 cannot
    /// encode. §11.3.3.1 binds the keyword to Latin-1 in *all three* text chunks, so unlike the
    /// text — which §11.3.3.2 routes to `iTXt` — there is no chunk that could carry it.
    TextKeywordNotLatin1 = 2,
    /// A text annotation left behind because its keyword is empty or longer than the 79 bytes
    /// §11.3.3.1 allows. All three chunks fix that field at 1–79 bytes, so a reader — this
    /// crate's own included — drops the whole chunk rather than reading a longer one.
    TextKeywordLength = 3,
    /// A text annotation **written**, whose keyword leaves the repertoire §11.3.3.1 recommends
    /// ("only code points 0x20-7E and 0xA1-FF are allowed", and expressly "nor is U+00A0
    /// NON-BREAKING SPACE"). The keyword is written exactly as it arrived — this crate reads it
    /// back unchanged — but the datastream is then non-conforming per §15.3.1, which requires
    /// that "All field values in the PNG datastream obey the relationships specified in this
    /// specification".
    TextKeywordRepertoire = 4,
    /// A text annotation **written**, whose keyword has a leading, trailing or consecutive
    /// space, which §11.3.3.1 says are "not permitted in keywords" so that one keyword cannot be
    /// misread as another. Written as it arrived, for the same reason as
    /// [`TextKeywordRepertoire`](Self::TextKeywordRepertoire).
    TextKeywordSpacing = 5,
    /// A text annotation **written without its `iTXt` language tag**, because the tag was not
    /// the ASCII shape §11.3.3.4 requires ("a well-formed language tag defined by \[BCP47\]").
    /// Written as UTF-8 into a field a reader takes as Latin-1 the tag would not survive the
    /// trip; an empty tag is §11.3.3.4's own way of saying the language is unspecified.
    ItxtLanguageTag = 6,
    // 7 is retired, never to be reused: it was `XmpNotUtf8`, a packet handed over as bytes that
    // were not UTF-8. The packet is now an ordinary `TextChunk` (issue #600), whose text is a
    // `String`, so no path into the encoder can carry such bytes any more.
    /// A text payload left behind because its **text string** holds a null character, which
    /// §11.3.3.2 ("Neither the keyword nor the text string may contain a null character") and
    /// §11.3.3.4 ("neither shall contain a zero byte") both forbid.
    ///
    /// Unlike a null in a keyword this re-frames nothing — the text is last and "not
    /// null-terminated (the length of the chunk defines the ending)" — so it does not fail the
    /// encode. It is not written either: readers disagree about what such a chunk holds, libpng
    /// truncating the text at the null where this crate's reader returns it whole, so the
    /// payload is dropped rather than written into a file whose meaning depends on who reads it.
    ///
    /// The payload is usually a text annotation, but the XMP packet goes into an `iTXt` too and
    /// so can land here. That case is worth reading twice: XML 1.0 does not admit U+0000 in a
    /// document at all, so a packet that reaches this notice is not merely unwritable — it is
    /// already not well-formed XML, whatever produced it.
    TextStringNull = 8,
    /// An ICC profile left behind because its **profile name** holds a character Latin-1 cannot
    /// encode. §11.3.2.3 gives the name the keyword's rule — "Profile names shall contain only
    /// printable Latin-1 characters and spaces" — so there are no bytes that would read back as
    /// the name given.
    IccpNameNotLatin1 = 9,
    /// An ICC profile left behind because its profile name is empty or longer than the 79 bytes
    /// §11.3.2.3 allows ("Profile name — 1-79 bytes"). A reader — this crate's own included —
    /// refuses an `iCCP` whose name field is outside that range, so the profile would be lost on
    /// the next read.
    IccpNameLength = 10,
    /// An ICC profile left behind because its profile name holds a null character. The name
    /// ends at its null separator (§11.3.2.3), so the byte after an embedded one would be read as
    /// the compression method, and a reader refuses the chunk.
    IccpNameNull = 11,
    /// An ICC profile **written**, whose profile name leaves the repertoire §11.3.2.3 allows
    /// ("only code points 0x20-7E and 0xA1-FF are allowed"). Written exactly as it arrived, as
    /// [`TextKeywordRepertoire`](Self::TextKeywordRepertoire) is, and non-conforming per §15.3.1
    /// for the same reason.
    IccpNameRepertoire = 12,
    /// An ICC profile **written**, whose profile name has a leading, trailing or consecutive
    /// space, which §11.3.2.3 says "are not permitted". Written as it arrived, as
    /// [`TextKeywordSpacing`](Self::TextKeywordSpacing) is.
    IccpNameSpacing = 13,
    /// A modification time (`tIME`) left behind because a field lies outside the range
    /// §11.3.5.1's Table 26 gives it: month 1–12, day 1–31, hour 0–23, minute 0–59, second 0–60.
    /// A reader — this crate's own included, and libpng's `png_set_tIME` — skips such a chunk as
    /// malformed, so writing it would lose the time on the next read with nothing said. Raised by
    /// [`with_time`](PngEncoder::with_time); a carry never raises it, because the reader never
    /// yields such a time.
    TimeOutOfRange = 14,
}

impl MetadataNotice {
    /// One line naming the payload and what happened to it, fit to show a user.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::NonRgbCicp => {
                "cICP: its matrix coefficients are not 0, which PNG requires (§11.3.2.6)"
            }
            Self::C2paManifestStore => {
                "C2PA manifest store: signed over the source bytes, so a copy would be invalid \
                 (C2PA 2.4 §A.3.2) — re-sign the output"
            }
            Self::TextKeywordNotLatin1 => {
                "text annotation: its keyword is not Latin-1, which every text chunk requires \
                 (§11.3.3.1)"
            }
            Self::TextKeywordLength => {
                "text annotation: its keyword is not 1 to 79 bytes, the length every text chunk \
                 fixes (§11.3.3.1)"
            }
            Self::TextKeywordRepertoire => {
                "text annotation: written, but its keyword leaves the code points 0x20-0x7E and \
                 0xA1-0xFF §11.3.3.1 recommends, so the datastream is non-conforming per \
                 §15.3.1"
            }
            Self::TextKeywordSpacing => {
                "text annotation: written, but its keyword has a leading, trailing or \
                 consecutive space, which §11.3.3.1 does not permit"
            }
            Self::ItxtLanguageTag => {
                "text annotation: written without its language tag, which was not the BCP 47 \
                 shape §11.3.3.4 requires"
            }
            Self::TextStringNull => {
                "text annotation or XMP packet: its text string contains a null character, \
                 which no text chunk may hold (§11.3.3.2, §11.3.3.4)"
            }
            Self::IccpNameNotLatin1 => {
                "ICC profile: its profile name is not Latin-1, which iCCP requires (§11.3.2.3)"
            }
            Self::IccpNameLength => {
                "ICC profile: its profile name is not 1 to 79 bytes, the length iCCP fixes \
                 (§11.3.2.3)"
            }
            Self::IccpNameNull => {
                "ICC profile: its profile name contains a null character, which ends the name \
                 field (§11.3.2.3)"
            }
            Self::IccpNameRepertoire => {
                "ICC profile: written, but its profile name leaves the code points 0x20-0x7E and \
                 0xA1-0xFF §11.3.2.3 allows, so the datastream is non-conforming per §15.3.1"
            }
            Self::IccpNameSpacing => {
                "ICC profile: written, but its profile name has a leading, trailing or \
                 consecutive space, which §11.3.2.3 does not permit"
            }
            Self::TimeOutOfRange => {
                "modification time: a field is outside the range tIME gives it (§11.3.5.1, \
                 Table 26)"
            }
        }
    }

    /// Whether the payload still reached the output.
    ///
    /// `false` means it was left behind entirely; `true` means it was written, with the caveat
    /// [`reason`](Self::reason) gives. A caller showing these to a user needs the difference —
    /// "this did not come along" and "this came along in a form some readers dislike" call for
    /// different action.
    #[must_use]
    pub fn carried(self) -> bool {
        matches!(
            self,
            Self::TextKeywordRepertoire
                | Self::TextKeywordSpacing
                | Self::ItxtLanguageTag
                | Self::IccpNameRepertoire
                | Self::IccpNameSpacing
        )
    }
}

impl core::fmt::Display for MetadataNotice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.reason())
    }
}

impl PngEncoder {
    /// Carries every metadata chunk a [`PngMetadata`] holds into this encoder, so that
    /// re-encoding a file keeps its EXIF, ICC profile, XMP packet, text annotations, colour
    /// chunks, significant bits, background, physical dimensions and modification time instead of
    /// dropping them.
    ///
    /// This is the write-side counterpart of [`metadata`](crate::metadata): read a file's
    /// metadata without touching its pixels, then hand it to the encoder that rewrites them.
    /// A full decode carries the same value as [`DecodedPng::metadata`](crate::DecodedPng::metadata),
    /// so a re-encode of decoded pixels passes `&decoded.metadata` here.
    ///
    /// Calling it twice with the same metadata is the same as calling it once: a later carry
    /// replaces what an earlier one contributed rather than appending a second copy of every
    /// annotation.
    ///
    /// # What it carries, and what it deliberately does not
    ///
    /// Everything the read side surfaces is set, including a `cICP`, an `sRGB` and an `iCCP`
    /// together — §4.3 Table 1 ranks the colour chunks precisely so a file may carry more than
    /// one, and a reader honours the lowest priority number. That is a claim about *other*
    /// readers: this crate's own reader surfaces all of them and ranks none, because which chunk
    /// to honour depends on whether the reader has a colour-management module, which an encoder
    /// cannot know. Resolving a profile against an intent belongs to `gamut-cmm`. Each text annotation goes back into
    /// the chunk it came out of, compressed if it was compressed
    /// ([`TextChunkKind`]) and in its place among the others. The XMP
    /// packet is one of those annotations, so it keeps its compression flag, language tag,
    /// translated keyword and position the same way, and replaces a packet set earlier with
    /// [`with_xmp`](Self::with_xmp).
    ///
    /// Two payloads cannot be carried at all, and neither is dropped in silence — read them back
    /// with [`metadata_notices`](Self::metadata_notices):
    ///
    /// - a **`cICP` whose matrix coefficients are not 0**, which §11.3.2.6 does not allow in PNG;
    /// - the **C2PA manifest store**, signed over the bytes of the file it was made for.
    ///
    /// A text annotation (the XMP packet included) that §11.3.3 does not endorse is reported
    /// through the same channel rather than failing the carry: a keyword outside §11.3.3.1's
    /// repertoire or spacing rules is written as it arrived, while a keyword no chunk can hold
    /// and a text string holding a null are left behind, and
    /// [`MetadataNotice::carried`](MetadataNotice::carried) says which happened. **Only a null in
    /// a keyword** — or in an `iTXt` translated keyword — fails the encode with
    /// an [`ErrorKind::InvalidInput`](gamut_core::ErrorKind::InvalidInput) error naming the
    /// annotation: those fields end at their first null, so
    /// the chunk would be read back as a *different* annotation, which no notice can undo.
    ///
    /// The ICC profile's name answers to the same rule (§11.3.2.3); see
    /// [`with_icc_profile_name`](Self::with_icc_profile_name). A name read out of a file is already Latin-1,
    /// 1–79 bytes and null-free, so a carried profile is always written, with a notice if its
    /// name breaks the repertoire or spacing rule.
    ///
    /// `pHYs` and `tIME` are carried byte for byte. `tIME` is the time of the last image
    /// modification (§11.3.5.1), so a re-encode that changes the pixels should stamp its own:
    /// call [`with_time`](Self::with_time) after this method, which replaces the carried one.
    ///
    /// `sBIT` and `bKGD` are shaped by the colour type, so they go through the same conversion
    /// as their setters ([`with_significant_bits`](Self::with_significant_bits),
    /// [`with_background`](Self::with_background)): emitted for the
    /// colour type and depth actually written, and omitted, without a notice, where that header
    /// cannot carry them — the omission is decided at encode time, after the notices are read.
    /// A palette-index background ([`Background::Index`](crate::Background::Index)) names an
    /// entry of the source's palette, so it is carried only by
    /// [`encode_indexed8`](Self::encode_indexed8) with that palette.
    #[must_use]
    pub fn with_png_metadata(mut self, metadata: &PngMetadata) -> Self {
        self.carry_notices.clear();
        self.ancillary.begin_carry();
        if let Some(exif) = &metadata.exif {
            self = self.with_exif(exif);
        }
        // Both colour statements are carried. §5.6 Table 5 and §11.3.2.5 only *recommend* against
        // the pair, and §4.3 Table 1 exists to resolve it: `iCCP` outranks `sRGB`, so the profile
        // is what a reader applies and the intent is what a reader without a CMM falls back on.
        // Dropping either would throw away colour information the source carried.
        if let Some(icc) = &metadata.icc_profile {
            self = self
                .with_icc_profile_name(&icc.name)
                .with_icc_profile(&icc.profile);
        }
        if let Some(intent) = metadata.srgb {
            self = self.with_srgb(intent);
        }
        // A source chunk whose matrix coefficients are not 0 is not a conforming cICP (§11.3.2.6);
        // `with_cicp` leaves it behind with a notice rather than carry the defect forward.
        if let Some(cicp) = metadata.cicp {
            self = self.with_cicp(cicp);
        }
        if metadata.c2pa.is_some() {
            self.carry_notices.push(MetadataNotice::C2paManifestStore);
        }
        if let Some(gamma) = metadata.gamma {
            self = self.with_gamma(gamma);
        }
        if let Some(chromaticities) = metadata.chromaticities {
            self = self.with_chromaticities(chromaticities);
        }
        // Through the same setters a caller uses, so each is emitted for the header actually
        // written exactly as a caller-set one is (`written::sbit_for` / `bkgd_for`). The read
        // side surfaces the background in the decoded pixels' units, which are the units the
        // `with_background` takes.
        if let Some(bits) = &metadata.significant_bits {
            self = self.with_significant_bits(bits);
        }
        if let Some(background) = metadata.background {
            self = self.with_background(background);
        }
        if let Some(phys) = metadata.physical_dimensions {
            self = self.with_physical_dimensions(phys);
        }
        if let Some(time) = metadata.modification_time {
            self = self.with_time(time);
        }
        // The XMP packet is an ordinary annotation and goes back through the loop below, in its
        // file position and with its own chunk's framing. A packet set directly with `with_xmp`
        // is displaced by a carried one, as one carry's packet is by the next: the encoder
        // writes the packet it was last handed, and a source that holds none leaves it alone.
        // `end_carry` decides that on the entries as written, so a promoted packet counts too.
        for text in &metadata.texts {
            let (language, translated) = (
                text.language.as_deref().unwrap_or_default(),
                text.translated_keyword.as_deref().unwrap_or_default(),
            );
            match text.kind {
                TextChunkKind::Text => self.ancillary.add_text_latin1(&text.keyword, &text.text),
                TextChunkKind::CompressedText => {
                    self.ancillary
                        .add_text_compressed(&text.keyword, &text.text);
                }
                TextChunkKind::International => self.ancillary.add_text_international_tagged(
                    &text.keyword,
                    language,
                    translated,
                    &text.text,
                    false,
                ),
                TextChunkKind::CompressedInternational => {
                    self.ancillary.add_text_international_tagged(
                        &text.keyword,
                        language,
                        translated,
                        &text.text,
                        true,
                    );
                }
            }
        }
        self.ancillary.end_carry();
        self
    }

    /// What this encoder could not carry faithfully: whole payloads left behind, then the
    /// per-annotation notices, in the order they were found — empty when everything came along
    /// intact.
    ///
    /// Surface this to whoever asked for the re-encode. Losing metadata without saying so is the
    /// defect the preservation path exists to remove; losing it — or bending it — *with* an
    /// explanation is a choice the spec forces. Use
    /// [`MetadataNotice::carried`](MetadataNotice::carried) to tell the two apart.
    ///
    /// The `cICP` notice (§11.3.2.6) comes first and belongs to the code points last given to
    /// [`with_cicp`](Self::with_cicp), directly or by a carry. The payload-level notices describe
    /// the last [`with_png_metadata`](Self::with_png_metadata) call and are reset by each; the
    /// per-annotation notices belong to the annotations still accumulated, so they follow the
    /// same replace-not-append rule a carry gives the text list. The ICC profile's name notice
    /// (§11.3.2.3) sits between the two and belongs to the profile and name last set, by
    /// [`with_icc_profile`](Self::with_icc_profile) / [`with_icc_profile_name`](Self::with_icc_profile_name)
    /// or a carry; the modification time's (§11.3.5.1) follows it and belongs to the time last
    /// given to [`with_time`](Self::with_time).
    #[must_use]
    pub fn metadata_notices(&self) -> Vec<MetadataNotice> {
        let mut notices: Vec<MetadataNotice> = self.ancillary.cicp_notice().into_iter().collect();
        notices.extend(self.carry_notices.iter().copied());
        notices.extend(self.ancillary.iccp_notice());
        notices.extend(self.ancillary.time_notice());
        notices.extend(self.ancillary.text_notices());
        notices
    }
}
