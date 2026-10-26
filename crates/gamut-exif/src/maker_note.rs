//! The vendor-specific `MakerNote` block (Exif tag `0x927C`).
//!
//! `MakerNote` is an `UNDEFINED` blob of vendor-proprietary data — usually a mini-IFD with
//! per-vendor quirks (its own header, byte order, and offset base). The crate preserves the bytes
//! verbatim, detects the vendor (from the `Make` tag), and resolves the block's **dispatch layout**
//! — which header dialect it is, the byte order and position of its IFD, and what that IFD's value
//! offsets count from. On top of that layout sit read-only per-vendor views:
//!
//! - **Canon** — [`canon::CanonMakerNote`], through [`MakerNote::canon`] (issue #977).
//! - **Leica** — [`leica::LeicaMakerNote`], through [`MakerNote::leica`] (issue #978).
//! - **Sony** — [`sony::SonyMakerNote`], through [`MakerNote::sony`] (issue #980), for the
//!   [`Sony2`](MakerNoteDialect::Sony2) dialect; a [`Sony1`](MakerNoteDialect::Sony1) note has no
//!   view yet (issue #1033).
//!
//! Every other vendor's tags stay undecoded; their views are the remainder of issue #418 and can
//! be added without breaking this API (every enum and the layout struct here are
//! `#[non_exhaustive]`). A view only reads: the block's bytes are never rewritten from it.
//!
//! # Dispatch
//!
//! [`MakerNoteLayout::detect`] follows exiv2's dispatch table (`TiffMnCreator::registry_` and the
//! `*MnHeader` classes in `third_party/exiv2/src/makernote_int.cpp`; summarised at
//! <https://exiv2.org/makernote.html>), plus Apple and Leica, which exiv2 does not handle
//! (ExifTool's `MakerNoteApple` and `MakerNoteLeica*` entries; Leica's are staked in
//! `references/exif/leica/README.md`). The vendor picks the candidate dialects and the note's own
//! signature picks among them. See `references/exif/README.md` for the per-dialect table and where this reader
//! deliberately differs from exiv2. A note no dialect recognises has no layout and stays opaque.
//!
//! # Round-trip behaviour
//!
//! The *bytes* always round-trip exactly (value-level); resolving a layout reads them and changes
//! nothing. Additionally, a model that came from [`Exif::parse`](crate::Exif::parse) records the
//! note's absolute source offset ([`Exif::maker_note_offset`](crate::Exif::maker_note_offset)),
//! and the writer **pins** the note's byte range at that exact position on a rewrite — so
//! TIFF-header-absolute internal offsets a vendor encodes ([`OffsetBase::TiffHeader`]: Canon and
//! most others) stay valid even when edits shift the surrounding directories (issue #263). Only
//! when the new layout makes the pin unsatisfiable (the directory region grows past the note's old
//! position) does the writer fall back to relocating the block, in which case such vendor-internal
//! offsets may go stale. Note-relative offsets ([`OffsetBase::MakerNote`]) survive a relocation
//! too.

pub mod canon;
pub mod leica;
pub mod sony;

use gamut_ifd::{AuditFinding, ByteOrder, Ifd, IfdReader, RawIfd, ReadAt, Rebased, Variant};

use crate::error::Result;

/// The bytes a one-entry IFD occupies before its next-IFD pointer: a 2-byte entry count plus one
/// 12-byte entry. A note shorter than its header plus this holds no IFD to decode.
const ONE_ENTRY_IFD: usize = 14;

/// A decoded-only-as-far-as-the-vendor MakerNote block: its detected vendor and its raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MakerNote {
    /// The vendor whose dialect the block follows (detected from `Make`; [`MakerNoteVendor::Unknown`]
    /// if unrecognised).
    pub vendor: MakerNoteVendor,
    /// The opaque, verbatim block bytes.
    pub bytes: Vec<u8>,
}

impl MakerNote {
    /// The block's dispatch layout, given `outer` — the byte order of the TIFF stream the note sits
    /// in ([`Exif::byte_order`](crate::Exif::byte_order)), which most dialects inherit.
    ///
    /// `None` when the vendor has no known dialect or the bytes match none of its signatures; the
    /// block then stays opaque. See [`MakerNoteLayout::detect`].
    #[must_use]
    pub fn layout(&self, outer: ByteOrder) -> Option<MakerNoteLayout> {
        MakerNoteLayout::detect(self.vendor, &self.bytes, outer)
    }

    /// The block decoded as a Canon note, given `outer` (as for [`layout`](Self::layout)) and
    /// `note_at`, the note's position in its TIFF stream
    /// ([`Exif::maker_note_offset`](crate::Exif::maker_note_offset)) — Canon's value offsets
    /// count from the TIFF header, so they mean nothing without it.
    ///
    /// Reads only these bytes, so a value whose offset points outside the note is unreadable here
    /// and named by [`CanonMakerNote::unreadable_tags`](canon::CanonMakerNote::unreadable_tags);
    /// [`CanonMakerNote::read`](canon::CanonMakerNote::read) reads through the whole stream
    /// instead.
    ///
    /// `Ok(None)` when the block has no [`MakerNoteDialect::Canon`] layout.
    ///
    /// # Errors
    ///
    /// [`ExifError::Ifd`](crate::ExifError::Ifd) when the note's IFD itself — its entry count,
    /// entries and next-IFD pointer — does not fit in the note.
    pub fn canon(&self, outer: ByteOrder, note_at: u64) -> Result<Option<canon::CanonMakerNote>> {
        let Some(layout) = self.layout(outer) else {
            return Ok(None);
        };
        let placed = Placed {
            bytes: &self.bytes,
            at: note_at,
        };
        canon::CanonMakerNote::read(placed, note_at, layout)
    }

    /// The block decoded as a Leica note, given `outer` and `note_at` as for
    /// [`canon`](Self::canon): some Leica dialects count their value offsets from the TIFF header,
    /// the others from the note's start.
    ///
    /// Reads only these bytes, so a value whose offset points outside the note is unreadable here
    /// and named by [`LeicaMakerNote::unreadable_tags`](leica::LeicaMakerNote::unreadable_tags);
    /// [`LeicaMakerNote::read`](leica::LeicaMakerNote::read) reads through the whole stream
    /// instead.
    ///
    /// `Ok(None)` when the block has no layout, or one that is not a Leica IFD dialect (see
    /// [`LeicaMakerNote::read`](leica::LeicaMakerNote::read)).
    ///
    /// # Errors
    ///
    /// [`ExifError::Ifd`](crate::ExifError::Ifd) when the note's IFD itself — its entry count,
    /// entries and next-IFD pointer — does not fit in the note.
    pub fn leica(&self, outer: ByteOrder, note_at: u64) -> Result<Option<leica::LeicaMakerNote>> {
        let Some(layout) = self.layout(outer) else {
            return Ok(None);
        };
        let placed = Placed {
            bytes: &self.bytes,
            at: note_at,
        };
        leica::LeicaMakerNote::read(placed, note_at, layout)
    }

    /// The block decoded as a Sony note, given `outer` and `note_at` as for
    /// [`canon`](Self::canon): Sony's value offsets count from the TIFF header.
    ///
    /// Reads only these bytes, so a value whose offset points outside the note is unreadable here
    /// and named by [`SonyMakerNote::unreadable_tags`](sony::SonyMakerNote::unreadable_tags);
    /// [`SonyMakerNote::read`](sony::SonyMakerNote::read) reads through the whole stream instead.
    ///
    /// `Ok(None)` when the block has no [`MakerNoteDialect::Sony2`] layout — a
    /// [`Sony1`](MakerNoteDialect::Sony1) note included, which has no view yet (issue #1033).
    ///
    /// # Errors
    ///
    /// [`ExifError::Ifd`](crate::ExifError::Ifd) when the note's IFD itself — its entry count,
    /// entries and next-IFD pointer — does not fit in the note.
    pub fn sony(&self, outer: ByteOrder, note_at: u64) -> Result<Option<sony::SonyMakerNote>> {
        let Some(layout) = self.layout(outer) else {
            return Ok(None);
        };
        let placed = Placed {
            bytes: &self.bytes,
            at: note_at,
        };
        sony::SonyMakerNote::read(placed, note_at, layout)
    }
}

/// A note's bytes standing at `at` in the TIFF stream's offset space: offset `o` reads byte
/// `o - at` of the note, and every offset before the note is out of bounds.
///
/// The opposite of [`gamut_ifd::Rebased`], which maps `o` *forward* to `base + o`: a note whose
/// offsets count from the TIFF header needs the stream's offsets mapped *back* into the note, and
/// the [`Exif`](crate::Exif) model keeps the note's bytes, not the stream around them.
struct Placed<'a> {
    bytes: &'a [u8],
    at: u64,
}

impl ReadAt for Placed<'_> {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> gamut_core::Result<()> {
        let within = offset.checked_sub(self.at).ok_or_else(|| {
            gamut_core::Error::invalid_input(
                env!("CARGO_PKG_NAME"),
                "MakerNote: read before the note",
            )
            .with_byte_offset(offset)
        })?;
        self.bytes.read_exact_at(within, buf)
    }

    fn len(&mut self) -> gamut_core::Result<u64> {
        // The stream is at least as long as the note's end; saturating keeps a hostile `at` from
        // wrapping the length below the note's own position.
        Ok(self.at.saturating_add(self.bytes.len() as u64))
    }
}

/// Reads the IFD a note laid out as `layout` holds, out of `stream` — the note's TIFF stream, or
/// any source in that stream's offset space — where the note starts at `note_at`.
///
/// File-absolute offsets ([`OffsetBase::TiffHeader`]) are read through `stream` directly;
/// note-relative ones through a [`gamut_ifd::Rebased`] view at the base's
/// [`origin`](OffsetBase::origin). The directory is decoded leniently: an entry whose value cannot
/// be read is left out, and its tag returned beside the directory, in on-disk order. A tag the
/// note repeats keeps its last readable occurrence.
///
/// The directory is read with its 4-byte next-IFD pointer, so this serves only the dialects whose
/// [`has_next_ifd`](MakerNoteDialect::has_next_ifd) holds; the pointer itself is not followed.
///
/// # Errors
///
/// [`ExifError::Ifd`](crate::ExifError::Ifd) when the directory does not fit in `stream` or
/// `stream` fails, and [`ExifError::BadOffset`](crate::ExifError::BadOffset) when the IFD's
/// position overflows `u64` or lies before the offset base.
pub(crate) fn read_note_ifd<S: ReadAt>(
    stream: S,
    note_at: u64,
    layout: MakerNoteLayout,
) -> Result<(Ifd, Vec<u16>)> {
    let (mut reader, raw) = open_note_ifd(stream, note_at, layout)?;
    decode_lenient(&mut reader, &raw)
}

/// Reads the note's directory raw, as [`read_note_ifd`] does before decoding it, and returns it
/// with the reader it came from: a reader over `stream` rebased at the offset base's origin, so
/// any further offset the note holds (a vendor subdirectory's) reads in the same space.
///
/// # Errors
///
/// As [`read_note_ifd`].
pub(crate) fn open_note_ifd<S: ReadAt>(
    stream: S,
    note_at: u64,
    layout: MakerNoteLayout,
) -> Result<(IfdReader<Rebased<S>>, RawIfd)> {
    let ifd_at = note_at
        .checked_add(u64::from(layout.ifd_offset))
        .ok_or(crate::ExifError::BadOffset(note_at))?;
    let origin = layout
        .offset_base
        .origin(note_at)
        .ok_or(crate::ExifError::BadOffset(note_at))?;
    let within = ifd_at
        .checked_sub(origin)
        .ok_or(crate::ExifError::BadOffset(ifd_at))?;
    let mut reader =
        IfdReader::with_layout(stream.rebased(origin), layout.byte_order, Variant::Classic);
    let raw = reader.read_ifd(within)?;
    Ok((reader, raw))
}

/// Decodes `raw` leniently: an entry whose value cannot be read is left out of the directory and
/// its tag returned beside it, in on-disk order. A tag `raw` repeats keeps its last readable
/// occurrence.
///
/// # Errors
///
/// [`ExifError::Ifd`](crate::ExifError::Ifd) only when the source fails (an I/O-kind error); an
/// unreadable value is never an error.
pub(crate) fn decode_lenient<S: ReadAt>(
    reader: &mut IfdReader<S>,
    raw: &RawIfd,
) -> Result<(Ifd, Vec<u16>)> {
    let (ifd, findings) = reader.decode_ifd_lenient(raw)?;
    let unreadable = findings
        .iter()
        .filter_map(|finding| match finding {
            AuditFinding::UnreadableEntry { tag, .. } => Some(*tag),
            _ => None,
        })
        .collect();
    Ok((ifd, unreadable))
}

/// A MakerNote vendor dialect.
///
/// `#[non_exhaustive]`: more vendors — and, later, per-vendor decoding — can be added without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MakerNoteVendor {
    /// Canon.
    Canon,
    /// Nikon.
    Nikon,
    /// Sony.
    Sony,
    /// Fujifilm.
    Fujifilm,
    /// Olympus / OM Digital.
    Olympus,
    /// Panasonic / Lumix.
    Panasonic,
    /// Apple.
    Apple,
    /// An unrecognised or absent vendor.
    Unknown,
    /// Minolta / Konica Minolta.
    Minolta,
    /// Pentax.
    Pentax,
    /// Ricoh (including Ricoh Imaging's Pentax-branded bodies).
    Ricoh,
    /// Samsung.
    Samsung,
    /// Sigma / Foveon.
    Sigma,
    /// Casio.
    Casio,
    /// Leica (`LEICA`, `Leica Camera AG`, `LEICA CAMERA AG`).
    Leica,
}

impl MakerNoteVendor {
    /// Detects the vendor from the `Make` tag.
    ///
    /// A case-insensitive substring match on the manufacturer string — enough to route to the
    /// vendor's dialects, without inspecting the block's own signature (that is
    /// [`MakerNoteLayout::detect`]'s job).
    #[must_use]
    pub fn detect(make: Option<&str>) -> MakerNoteVendor {
        let Some(make) = make else {
            return MakerNoteVendor::Unknown;
        };
        let make = make.to_ascii_uppercase();
        let has = |needle: &str| make.contains(needle);
        if has("CANON") {
            MakerNoteVendor::Canon
        } else if has("NIKON") {
            MakerNoteVendor::Nikon
        } else if has("SONY") {
            MakerNoteVendor::Sony
        } else if has("FUJI") {
            MakerNoteVendor::Fujifilm
        } else if has("OLYMPUS") || has("OM DIGITAL") {
            MakerNoteVendor::Olympus
        } else if has("PANASONIC") || has("LUMIX") {
            MakerNoteVendor::Panasonic
        } else if has("APPLE") {
            MakerNoteVendor::Apple
        } else if has("MINOLTA") {
            MakerNoteVendor::Minolta
        } else if has("PENTAX") {
            MakerNoteVendor::Pentax
        } else if has("RICOH") {
            MakerNoteVendor::Ricoh
        } else if has("SAMSUNG") {
            MakerNoteVendor::Samsung
        } else if has("SIGMA") || has("FOVEON") {
            MakerNoteVendor::Sigma
        } else if has("CASIO") {
            MakerNoteVendor::Casio
        } else if has("LEICA") {
            MakerNoteVendor::Leica
        } else {
            MakerNoteVendor::Unknown
        }
    }
}

/// The header variant a MakerNote follows within its vendor, named after exiv2's makernote groups
/// (Leica's, which exiv2 lacks, after ExifTool's `MakerNoteLeica*` entries).
///
/// The discriminants are permanent and append-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum MakerNoteDialect {
    /// Canon: a bare IFD at byte 0.
    Canon = 0,
    /// Nikon type 1 (early Coolpix): a bare IFD at byte 0, no `Nikon` signature.
    Nikon1 = 1,
    /// Nikon type 2: `Nikon\0` + 2 version bytes, IFD at byte 8.
    Nikon2 = 2,
    /// Nikon type 3: `Nikon\0` + 4 version bytes, then an embedded TIFF header at byte 10 whose
    /// byte order and first-IFD offset the note's IFD follows.
    Nikon3 = 3,
    /// Sony with the `SONY DSC \0\0\0` header: IFD at byte 12, no next-IFD pointer.
    Sony1 = 4,
    /// Sony without a header: a bare IFD at byte 0.
    Sony2 = 5,
    /// Fujifilm: `FUJIFILM` + a little-endian IFD offset; the whole note is little-endian.
    Fujifilm = 6,
    /// Olympus type 1: `OLYMP\0` + 2 version bytes, IFD at byte 8.
    Olympus1 = 7,
    /// Olympus type 2: `OLYMPUS\0` + a byte-order mark + 2 version bytes, IFD at byte 12.
    Olympus2 = 8,
    /// OM System: `OM SYSTEM\0\0\0` + a byte-order mark + 2 version bytes, IFD at byte 16.
    OmSystem = 9,
    /// Panasonic: `Panasonic\0\0\0`, IFD at byte 12, no next-IFD pointer.
    Panasonic = 10,
    /// Pentax (and Pentax-format Ricoh/Samsung): `AOC\0` + a byte-order mark, IFD at byte 6.
    Pentax = 11,
    /// Pentax in DNG: `PENTAX \0` + a byte-order mark, IFD at byte 10.
    PentaxDng = 12,
    /// Samsung (genuine Samsung bodies): a bare IFD at byte 0.
    Samsung = 13,
    /// Sigma / Foveon: `SIGMA\0\0\0` or `FOVEON\0\0` + 2 version bytes, IFD at byte 10.
    Sigma = 14,
    /// Casio type 1: a bare IFD at byte 0.
    Casio1 = 15,
    /// Casio type 2: `QVC\0\0\0`, IFD at byte 6; the whole note is big-endian.
    Casio2 = 16,
    /// Minolta / Konica Minolta: a bare IFD at byte 0.
    Minolta = 17,
    /// Apple: `Apple iOS\0` + 2 version bytes + a byte-order mark, IFD at byte 14.
    Apple = 18,
    /// Leica R8/R9 (ExifTool's `MakerNoteLeica3`): a bare IFD at byte 0, offsets from the TIFF
    /// header.
    Leica3 = 19,
    /// Leica M9 / M Monochrom (`MakerNoteLeica4`): `LEICA0`, IFD at byte 8, offsets from the note's
    /// start.
    Leica4 = 20,
    /// Leica X1/X2/X Vario/T/X/X-U (`MakerNoteLeica5`): `LEICA\0` + a version byte + `\0`, IFD at
    /// byte 8, offsets from the note's start.
    Leica5 = 21,
    /// Leica Q/SL/CL (`MakerNoteLeica8`): `LEICA\0` + a version byte + `\0`, IFD at byte 8, offsets
    /// from the TIFF header.
    Leica8 = 22,
    /// Leica M10/S (`MakerNoteLeica9`): `LEICA\0\x02\0`, IFD at byte 8, offsets from the TIFF
    /// header.
    Leica9 = 23,
    /// Leica D-Lux 7 (`MakerNoteLeica10`): `LEICA CAMERA AG\0`, a Panasonic-format IFD at byte 18
    /// with no next-IFD pointer, offsets from the TIFF header.
    Leica10 = 24,
}

impl MakerNoteDialect {
    /// Whether the note's IFD ends in a 4-byte next-IFD pointer. Panasonic and Sony type 1 omit it
    /// (exiv2 builds both with `hasNext = false`), and so does Leica 10, a Panasonic-format note;
    /// every other dialect carries one.
    #[must_use]
    pub fn has_next_ifd(self) -> bool {
        !matches!(
            self,
            MakerNoteDialect::Panasonic | MakerNoteDialect::Sony1 | MakerNoteDialect::Leica10
        )
    }
}

/// What a MakerNote IFD's value offsets count from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OffsetBase {
    /// File-absolute: offsets count from the outer TIFF header, like the rest of the Exif stream.
    /// They stay valid on a rewrite only while the writer's pin holds the note in place.
    TiffHeader,
    /// Note-relative: offsets count from `at` bytes into the MakerNote value (`0` for most such
    /// dialects; `10`, the embedded TIFF header, for Nikon type 3).
    MakerNote {
        /// The base's position within the note.
        at: u32,
    },
}

impl OffsetBase {
    /// The base's absolute position in the TIFF stream, given `note_at`, the note's own position
    /// there ([`Exif::maker_note_offset`](crate::Exif::maker_note_offset)). `None` only if the sum
    /// overflows.
    #[must_use]
    pub fn origin(self, note_at: u64) -> Option<u64> {
        match self {
            OffsetBase::TiffHeader => Some(0),
            OffsetBase::MakerNote { at } => note_at.checked_add(u64::from(at)),
        }
    }
}

/// How a MakerNote block is laid out: the dispatch facts a vendor decoder needs before it reads a
/// single tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MakerNoteLayout {
    /// The header variant the note follows.
    pub dialect: MakerNoteDialect,
    /// The byte order of the note's IFD: inherited from the outer stream, fixed by the dialect, or
    /// read from a byte-order mark in the note's header.
    pub byte_order: ByteOrder,
    /// Where the note's IFD (its 2-byte entry count) starts, in bytes from the start of the note.
    pub ifd_offset: u32,
    /// What the IFD's value offsets count from.
    pub offset_base: OffsetBase,
}

impl MakerNoteLayout {
    fn new(
        dialect: MakerNoteDialect,
        byte_order: ByteOrder,
        ifd_offset: u32,
        offset_base: OffsetBase,
    ) -> MakerNoteLayout {
        MakerNoteLayout {
            dialect,
            byte_order,
            ifd_offset,
            offset_base,
        }
    }

    /// Resolves the layout of `note`, a `vendor` MakerNote inside a TIFF stream of byte order
    /// `outer`.
    ///
    /// `None` when the vendor has no known dialect ([`MakerNoteVendor::Unknown`] always), when the
    /// note matches none of the vendor's signatures, or when it is too short to hold its header
    /// plus a one-entry IFD at [`ifd_offset`](Self::ifd_offset). Never reads past `note`.
    #[must_use]
    pub fn detect(
        vendor: MakerNoteVendor,
        note: &[u8],
        outer: ByteOrder,
    ) -> Option<MakerNoteLayout> {
        use MakerNoteDialect as D;
        let bare = |dialect| MakerNoteLayout::new(dialect, outer, 0, OffsetBase::TiffHeader);
        let layout = match vendor {
            MakerNoteVendor::Canon => bare(D::Canon),
            MakerNoteVendor::Minolta => bare(D::Minolta),
            MakerNoteVendor::Nikon => nikon(note, outer)?,
            MakerNoteVendor::Sony if note.starts_with(b"SONY DSC \0\0\0") => {
                MakerNoteLayout::new(D::Sony1, outer, 12, OffsetBase::TiffHeader)
            }
            MakerNoteVendor::Sony => bare(D::Sony2),
            MakerNoteVendor::Fujifilm => fujifilm(note)?,
            MakerNoteVendor::Olympus => olympus(note, outer)?,
            MakerNoteVendor::Panasonic if note.starts_with(b"Panasonic") => {
                MakerNoteLayout::new(D::Panasonic, outer, 12, OffsetBase::TiffHeader)
            }
            MakerNoteVendor::Pentax | MakerNoteVendor::Ricoh => pentax(note, outer)?,
            MakerNoteVendor::Samsung => pentax(note, outer).unwrap_or(MakerNoteLayout::new(
                D::Samsung,
                outer,
                0,
                OffsetBase::MakerNote { at: 0 },
            )),
            MakerNoteVendor::Sigma
                if note.starts_with(b"SIGMA\0\0\0") || note.starts_with(b"FOVEON\0\0") =>
            {
                MakerNoteLayout::new(D::Sigma, outer, 10, OffsetBase::TiffHeader)
            }
            MakerNoteVendor::Casio if note.starts_with(b"QVC\0\0\0") => {
                MakerNoteLayout::new(D::Casio2, ByteOrder::BigEndian, 6, OffsetBase::TiffHeader)
            }
            MakerNoteVendor::Casio => bare(D::Casio1),
            MakerNoteVendor::Apple if note.starts_with(b"Apple iOS\0") => MakerNoteLayout::new(
                D::Apple,
                mark(note, 12).unwrap_or(outer),
                14,
                OffsetBase::MakerNote { at: 0 },
            ),
            MakerNoteVendor::Leica => leica(note, outer)?,
            _ => return None,
        };
        let ifd_end = usize::try_from(layout.ifd_offset)
            .ok()?
            .checked_add(ONE_ENTRY_IFD)?;
        (ifd_end <= note.len()).then_some(layout)
    }
}

/// The byte order a two-byte mark at `at` names: `II` little-endian, `MM` big-endian, anything
/// else (or a mark past the end of `note`) none.
fn mark(note: &[u8], at: usize) -> Option<ByteOrder> {
    match note.get(at..at + 2)? {
        b"II" => Some(ByteOrder::LittleEndian),
        b"MM" => Some(ByteOrder::BigEndian),
        _ => None,
    }
}

/// Nikon, as exiv2's `newNikonMn` splits it: no `Nikon\0` signature is type 1; the signature
/// followed by a classic TIFF header at byte 10 is type 3; the signature alone is type 2.
fn nikon(note: &[u8], outer: ByteOrder) -> Option<MakerNoteLayout> {
    use MakerNoteDialect as D;
    if !note.starts_with(b"Nikon\0") {
        return Some(MakerNoteLayout::new(
            D::Nikon1,
            outer,
            0,
            OffsetBase::TiffHeader,
        ));
    }
    let Some(Ok((order, Variant::Classic, first_ifd))) = note.get(10..).map(gamut_ifd::read_header)
    else {
        return Some(MakerNoteLayout::new(
            D::Nikon2,
            outer,
            8,
            OffsetBase::TiffHeader,
        ));
    };
    // A classic header's first-IFD offset is a u32, so only the `+ 10` can fail — and an IFD
    // that would start past 4 GiB into the note is no IFD this note holds.
    let ifd_offset = u32::try_from(first_ifd).ok()?.checked_add(10)?;
    Some(MakerNoteLayout::new(
        D::Nikon3,
        order,
        ifd_offset,
        OffsetBase::MakerNote { at: 10 },
    ))
}

/// Fujifilm: `FUJIFILM`, then the IFD's offset as a little-endian u32 at byte 8; the IFD and its
/// values are little-endian whatever the outer stream says.
fn fujifilm(note: &[u8]) -> Option<MakerNoteLayout> {
    if !note.starts_with(b"FUJIFILM") {
        return None;
    }
    let ifd_offset = u32::from_le_bytes(note.get(8..12)?.try_into().ok()?);
    Some(MakerNoteLayout::new(
        MakerNoteDialect::Fujifilm,
        ByteOrder::LittleEndian,
        ifd_offset,
        OffsetBase::MakerNote { at: 0 },
    ))
}

/// Olympus and OM System: the two newer headers carry a byte-order mark, which this reader
/// requires and follows; the oldest carries none and inherits.
fn olympus(note: &[u8], outer: ByteOrder) -> Option<MakerNoteLayout> {
    use MakerNoteDialect as D;
    let note_base = OffsetBase::MakerNote { at: 0 };
    if note.starts_with(b"OM SYSTEM\0\0\0") {
        Some(MakerNoteLayout::new(
            D::OmSystem,
            mark(note, 12)?,
            16,
            note_base,
        ))
    } else if note.starts_with(b"OLYMPUS\0") {
        Some(MakerNoteLayout::new(
            D::Olympus2,
            mark(note, 8)?,
            12,
            note_base,
        ))
    } else if note.starts_with(b"OLYMP\0") {
        Some(MakerNoteLayout::new(
            D::Olympus1,
            outer,
            8,
            OffsetBase::TiffHeader,
        ))
    } else {
        None
    }
}

/// Pentax-format notes (Pentax, Ricoh, and Pentax-built Samsung bodies): `PENTAX \0` in DNG,
/// `AOC\0` elsewhere, each followed by a byte-order mark the note's IFD follows when it is one.
fn pentax(note: &[u8], outer: ByteOrder) -> Option<MakerNoteLayout> {
    use MakerNoteDialect as D;
    if note.starts_with(b"PENTAX \0") {
        Some(MakerNoteLayout::new(
            D::PentaxDng,
            mark(note, 8).unwrap_or(outer),
            10,
            OffsetBase::MakerNote { at: 0 },
        ))
    } else if note.starts_with(b"AOC\0") {
        Some(MakerNoteLayout::new(
            D::Pentax,
            mark(note, 4).unwrap_or(outer),
            6,
            OffsetBase::TiffHeader,
        ))
    } else {
        None
    }
}

/// Leica, after ExifTool's `MakerNoteLeica*` entries (`references/exif/leica/README.md`), as far
/// as the note's own bytes decide them: Fujifilm's signature (Leica's Fujifilm-built bodies), a
/// `LEICA` signature naming one of the dialects below, or no `LEICA` signature at all (a bare IFD).
/// Every Leica IFD inherits the outer byte order. A `LEICA` signature whose bytes fit none of them
/// — `LEICA\0\0\0` and `LEICA\0\x02\xff` among them, whose offset base ExifTool settles from
/// `Make` and `Model` — has no layout.
fn leica(note: &[u8], outer: ByteOrder) -> Option<MakerNoteLayout> {
    use MakerNoteDialect as D;
    let layout = |dialect, ifd_offset, offset_base| {
        Some(MakerNoteLayout::new(
            dialect,
            outer,
            ifd_offset,
            offset_base,
        ))
    };
    let note_start = OffsetBase::MakerNote { at: 0 };
    let tiff = OffsetBase::TiffHeader;
    if note.starts_with(b"FUJIFILM") {
        return fujifilm(note);
    }
    if note.starts_with(b"LEICA CAMERA AG\0") {
        return layout(D::Leica10, 18, tiff);
    }
    if note.starts_with(b"LEICA0") {
        return layout(D::Leica4, 8, note_start);
    }
    if !note.starts_with(b"LEICA") {
        return layout(D::Leica3, 0, tiff);
    }
    match note.get(5..8)? {
        [0, 0x01 | 0x04..=0x07 | 0x10 | 0x1a, 0] => layout(D::Leica5, 8, note_start),
        [0, 0x08..=0x0a, 0] => layout(D::Leica8, 8, tiff),
        [0, 0x02, 0] => layout(D::Leica9, 8, tiff),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use ByteOrder::{BigEndian as BE, LittleEndian as LE};
    use MakerNoteDialect as D;
    use MakerNoteVendor as V;

    use super::*;

    /// `header` followed by a one-entry IFD and its next-IFD pointer — the shortest note every
    /// dialect accepts once its header matches.
    fn note(header: &[u8]) -> Vec<u8> {
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(&[0; ONE_ENTRY_IFD + 4]);
        bytes
    }

    /// The layout `vendor` resolves for `bytes` inside a little-endian and a big-endian stream.
    fn both(vendor: MakerNoteVendor, bytes: &[u8]) -> [Option<MakerNoteLayout>; 2] {
        [LE, BE].map(|outer| MakerNoteLayout::detect(vendor, bytes, outer))
    }

    /// Asserts that `bytes` resolves, for `vendor` under both outer byte orders, to `dialect` at
    /// `ifd_offset` with `base`, and with the byte order `order` names (`None` = inherited).
    fn assert_layout(
        vendor: MakerNoteVendor,
        bytes: &[u8],
        dialect: MakerNoteDialect,
        order: Option<ByteOrder>,
        ifd_offset: u32,
        base: OffsetBase,
    ) {
        for (outer, got) in [LE, BE].into_iter().zip(both(vendor, bytes)) {
            let want = MakerNoteLayout::new(dialect, order.unwrap_or(outer), ifd_offset, base);
            assert_eq!(got, Some(want), "{vendor:?} under {outer:?}");
        }
    }

    const ABSOLUTE: OffsetBase = OffsetBase::TiffHeader;
    const RELATIVE: OffsetBase = OffsetBase::MakerNote { at: 0 };

    #[test]
    fn detects_vendors_case_insensitively() {
        let cases = [
            ("Canon", V::Canon),
            ("NIKON CORPORATION", V::Nikon),
            ("SONY", V::Sony),
            ("FUJIFILM", V::Fujifilm),
            ("OLYMPUS IMAGING CORP.", V::Olympus),
            ("OM Digital Solutions", V::Olympus),
            ("Panasonic", V::Panasonic),
            ("LUMIX", V::Panasonic),
            ("Apple", V::Apple),
            ("KONICA MINOLTA", V::Minolta),
            ("PENTAX Corporation", V::Pentax),
            ("RICOH IMAGING COMPANY, LTD.", V::Ricoh),
            ("SAMSUNG", V::Samsung),
            ("SIGMA", V::Sigma),
            ("FOVEON", V::Sigma),
            ("CASIO COMPUTER CO.,LTD.", V::Casio),
            ("Hasselblad", V::Unknown),
        ];
        for (make, vendor) in cases {
            assert_eq!(MakerNoteVendor::detect(Some(make)), vendor, "{make}");
        }
        assert_eq!(MakerNoteVendor::detect(None), V::Unknown);
    }

    #[test]
    fn bare_ifd_dialects_inherit_the_outer_order_at_byte_zero() {
        let bytes = note(b"");
        assert_layout(V::Canon, &bytes, D::Canon, None, 0, ABSOLUTE);
        assert_layout(V::Minolta, &bytes, D::Minolta, None, 0, ABSOLUTE);
        assert_layout(V::Nikon, &bytes, D::Nikon1, None, 0, ABSOLUTE);
        assert_layout(V::Sony, &bytes, D::Sony2, None, 0, ABSOLUTE);
        assert_layout(V::Casio, &bytes, D::Casio1, None, 0, ABSOLUTE);
        assert_layout(V::Samsung, &bytes, D::Samsung, None, 0, RELATIVE);
    }

    #[test]
    fn nikon_type_2_is_the_signature_without_an_embedded_tiff_header() {
        let bytes = note(b"Nikon\0\x01\x00");
        assert_layout(V::Nikon, &bytes, D::Nikon2, None, 8, ABSOLUTE);
        // A header with the wrong magic is not a TIFF header either.
        let bytes = note(b"Nikon\0\x02\x10\0\0II\x2b\0\x08\0\0\0");
        assert_layout(V::Nikon, &bytes, D::Nikon2, None, 8, ABSOLUTE);
    }

    #[test]
    fn nikon_type_3_follows_its_embedded_tiff_header() {
        let base = OffsetBase::MakerNote { at: 10 };
        let bytes = note(b"Nikon\0\x02\x10\0\0MM\0\x2a\0\0\0\x08");
        assert_layout(V::Nikon, &bytes, D::Nikon3, Some(BE), 18, base);
        let bytes = note(b"Nikon\0\x02\x10\0\0II\x2a\0\x08\0\0\0");
        assert_layout(V::Nikon, &bytes, D::Nikon3, Some(LE), 18, base);
        // The first-IFD offset is honoured, not assumed to be 8.
        let mut bytes = note(b"Nikon\0\x02\x10\0\0II\x2a\0\x0c\0\0\0");
        bytes.extend_from_slice(&[0; 4]);
        assert_layout(V::Nikon, &bytes, D::Nikon3, Some(LE), 22, base);
    }

    #[test]
    fn nikon_type_3_whose_ifd_lies_past_the_note_has_no_layout() {
        // 0xFFFF_FFFF + 10 does not fit a u32, so the header names no IFD the note can hold.
        let bytes = note(b"Nikon\0\x02\x10\0\0II\x2a\0\xff\xff\xff\xff");
        assert_eq!(both(V::Nikon, &bytes), [None, None]);
        // The largest offset that does fit resolves, and is then refused as past the note.
        let bytes = note(b"Nikon\0\x02\x10\0\0II\x2a\0\xf5\xff\xff\xff");
        assert_eq!(both(V::Nikon, &bytes), [None, None]);
    }

    #[test]
    fn sony_type_1_carries_its_header() {
        let bytes = note(b"SONY DSC \0\0\0");
        assert_layout(V::Sony, &bytes, D::Sony1, None, 12, ABSOLUTE);
    }

    #[test]
    fn fujifilm_is_little_endian_with_a_header_named_ifd_offset() {
        let bytes = note(b"FUJIFILM\x0c\0\0\0");
        assert_layout(V::Fujifilm, &bytes, D::Fujifilm, Some(LE), 12, RELATIVE);
        let mut bytes = note(b"FUJIFILM\x10\0\0\0");
        bytes.extend_from_slice(&[0; 4]);
        assert_layout(V::Fujifilm, &bytes, D::Fujifilm, Some(LE), 16, RELATIVE);
        // Signature without the offset field, and no signature at all.
        assert_eq!(both(V::Fujifilm, b"FUJIFILM\x0c\0\0"), [None, None]);
        assert_eq!(
            both(V::Fujifilm, &note(b"FUJIFILX\x0c\0\0\0")),
            [None, None]
        );
    }

    #[test]
    fn olympus_dialects_split_on_their_signatures() {
        let bytes = note(b"OLYMP\0\x01\0");
        assert_layout(V::Olympus, &bytes, D::Olympus1, None, 8, ABSOLUTE);
        let bytes = note(b"OLYMPUS\0II\x03\0");
        assert_layout(V::Olympus, &bytes, D::Olympus2, Some(LE), 12, RELATIVE);
        let bytes = note(b"OLYMPUS\0MM\x03\0");
        assert_layout(V::Olympus, &bytes, D::Olympus2, Some(BE), 12, RELATIVE);
        let bytes = note(b"OM SYSTEM\0\0\0II\x04\0");
        assert_layout(V::Olympus, &bytes, D::OmSystem, Some(LE), 16, RELATIVE);
        let bytes = note(b"OM SYSTEM\0\0\0MM\x04\0");
        assert_layout(V::Olympus, &bytes, D::OmSystem, Some(BE), 16, RELATIVE);
    }

    #[test]
    fn olympus_type_2_and_om_system_require_their_byte_order_mark() {
        assert_eq!(both(V::Olympus, &note(b"OLYMPUS\0XX\x03\0")), [None, None]);
        assert_eq!(
            both(V::Olympus, &note(b"OM SYSTEM\0\0\0XX\x04\0")),
            [None, None]
        );
        assert_eq!(both(V::Olympus, &note(b"OLYMQ\0\x01\0")), [None, None]);
    }

    #[test]
    fn panasonic_carries_its_header() {
        let bytes = note(b"Panasonic\0\0\0");
        assert_layout(V::Panasonic, &bytes, D::Panasonic, None, 12, ABSOLUTE);
        assert_eq!(both(V::Panasonic, &note(b"Panasonix\0\0\0")), [None, None]);
    }

    #[test]
    fn pentax_format_notes_follow_their_mark_or_inherit() {
        for vendor in [V::Pentax, V::Ricoh, V::Samsung] {
            let bytes = note(b"AOC\0MM");
            assert_layout(vendor, &bytes, D::Pentax, Some(BE), 6, ABSOLUTE);
            let bytes = note(b"AOC\0II");
            assert_layout(vendor, &bytes, D::Pentax, Some(LE), 6, ABSOLUTE);
            let bytes = note(b"AOC\0  ");
            assert_layout(vendor, &bytes, D::Pentax, None, 6, ABSOLUTE);
            let bytes = note(b"PENTAX \0MM");
            assert_layout(vendor, &bytes, D::PentaxDng, Some(BE), 10, RELATIVE);
            let bytes = note(b"PENTAX \0II");
            assert_layout(vendor, &bytes, D::PentaxDng, Some(LE), 10, RELATIVE);
        }
        for vendor in [V::Pentax, V::Ricoh] {
            assert_eq!(both(vendor, &note(b"Ricoh\0\0\0")), [None, None]);
        }
    }

    #[test]
    fn sigma_accepts_either_signature() {
        let bytes = note(b"SIGMA\0\0\0\x01\0");
        assert_layout(V::Sigma, &bytes, D::Sigma, None, 10, ABSOLUTE);
        let bytes = note(b"FOVEON\0\0\x01\0");
        assert_layout(V::Sigma, &bytes, D::Sigma, None, 10, ABSOLUTE);
        assert_eq!(both(V::Sigma, &note(b"SIGMB\0\0\0\x01\0")), [None, None]);
    }

    #[test]
    fn casio_type_2_is_big_endian() {
        let bytes = note(b"QVC\0\0\0");
        assert_layout(V::Casio, &bytes, D::Casio2, Some(BE), 6, ABSOLUTE);
    }

    #[test]
    fn apple_is_note_relative_and_follows_its_mark() {
        // The iPhone 12 Pro sample in `third_party/gamut-dng-samples` opens `Apple iOS\0\0\x01MM`.
        let bytes = note(b"Apple iOS\0\0\x01MM");
        assert_layout(V::Apple, &bytes, D::Apple, Some(BE), 14, RELATIVE);
        let bytes = note(b"Apple iOS\0\0\x01II");
        assert_layout(V::Apple, &bytes, D::Apple, Some(LE), 14, RELATIVE);
        let bytes = note(b"Apple iOS\0\0\x01\0\0");
        assert_layout(V::Apple, &bytes, D::Apple, None, 14, RELATIVE);
        assert_eq!(both(V::Apple, &note(b"Apple iOS!\0\x01MM")), [None, None]);
    }

    #[test]
    fn leica_is_any_make_naming_leica() {
        for make in ["LEICA", "Leica Camera AG", "LEICA CAMERA AG"] {
            assert_eq!(MakerNoteVendor::detect(Some(make)), V::Leica, "{make}");
        }
    }

    #[test]
    fn leica_signatures_pick_their_dialect() {
        // `Leica3`: anything not signed `LEICA` (or `FUJIFILM`) is a bare IFD.
        assert_layout(V::Leica, &note(b""), D::Leica3, None, 0, ABSOLUTE);
        assert_layout(V::Leica, &note(b"LEIC"), D::Leica3, None, 0, ABSOLUTE);
        // `Leica4`: `LEICA0`, whatever follows (the M Monochrom fixture opens `LEICA0\x03\0`).
        assert_layout(
            V::Leica,
            &note(b"LEICA0\x03\0"),
            D::Leica4,
            None,
            8,
            RELATIVE,
        );
        assert_layout(V::Leica, &note(b"LEICA0\0\0"), D::Leica4, None, 8, RELATIVE);
        // Each version byte separately: an or-pattern alternative is invisible to the gate.
        for version in [0x01, 0x04, 0x05, 0x06, 0x07, 0x10, 0x1a] {
            let bytes = note(&[b'L', b'E', b'I', b'C', b'A', 0, version, 0]);
            assert_layout(V::Leica, &bytes, D::Leica5, None, 8, RELATIVE);
        }
        for version in [0x08, 0x09, 0x0a] {
            let bytes = note(&[b'L', b'E', b'I', b'C', b'A', 0, version, 0]);
            assert_layout(V::Leica, &bytes, D::Leica8, None, 8, ABSOLUTE);
        }
        assert_layout(
            V::Leica,
            &note(b"LEICA\0\x02\0"),
            D::Leica9,
            None,
            8,
            ABSOLUTE,
        );
        let mut bytes = note(b"LEICA CAMERA AG\0\0\0");
        bytes.truncate(18 + ONE_ENTRY_IFD);
        assert_layout(V::Leica, &bytes, D::Leica10, None, 18, ABSOLUTE);
    }

    #[test]
    fn leica_signatures_the_bytes_cannot_decide_have_no_layout() {
        for header in [
            &b"LEICA\0\0\0"[..],
            b"LEICA\0\x02\xff",
            b"LEICA\0\x03\0",
            b"LEICA\0\x0b\0",
            b"LEICA\x01\x02\0",
            b"LEICA\0\x02\x01",
            b"LEICA CAMERA AG!",
        ] {
            assert_eq!(both(V::Leica, &note(header)), [None, None], "{header:?}");
        }
        // A signature cut short of its version bytes names no dialect.
        assert_eq!(both(V::Leica, b"LEICA\0\x02"), [None, None]);
    }

    #[test]
    fn leica_fujifilm_bodies_follow_the_fujifilm_dialect() {
        let bytes = note(b"FUJIFILM\x0c\0\0\0");
        assert_layout(V::Leica, &bytes, D::Fujifilm, Some(LE), 12, RELATIVE);
    }

    #[test]
    fn leica_10_must_hold_its_18_byte_header_and_a_one_entry_ifd() {
        let mut bytes = note(b"LEICA CAMERA AG\0\0\0");
        bytes.truncate(18 + ONE_ENTRY_IFD - 1);
        assert_eq!(both(V::Leica, &bytes), [None, None]);
    }

    #[test]
    fn unknown_vendors_have_no_layout() {
        assert_eq!(both(V::Unknown, &note(b"")), [None, None]);
    }

    #[test]
    fn a_note_must_hold_its_header_and_a_one_entry_ifd() {
        // Exactly the header plus a one-entry IFD resolves; one byte less does not.
        let fits = b"Panasonic\0\0\0".iter().copied().chain([0; ONE_ENTRY_IFD]);
        let fits: Vec<u8> = fits.collect();
        assert!(both(V::Panasonic, &fits).iter().all(Option::is_some));
        assert_eq!(both(V::Panasonic, &fits[..fits.len() - 1]), [None, None]);
        assert!(
            both(V::Canon, &[0; ONE_ENTRY_IFD])
                .iter()
                .all(Option::is_some)
        );
        assert_eq!(both(V::Canon, &[0; ONE_ENTRY_IFD - 1]), [None, None]);
        // A Fujifilm offset past the note is refused, never read.
        assert_eq!(
            both(V::Fujifilm, &note(b"FUJIFILM\xff\xff\xff\xff")),
            [None, None]
        );
    }

    #[test]
    fn only_panasonic_sony_type_1_and_leica_10_omit_the_next_ifd_pointer() {
        assert!(!D::Panasonic.has_next_ifd());
        assert!(!D::Sony1.has_next_ifd());
        assert!(!D::Leica10.has_next_ifd());
        assert!(D::Leica9.has_next_ifd());
        assert!(D::Sony2.has_next_ifd());
        assert!(D::Canon.has_next_ifd());
    }

    #[test]
    fn offset_base_origin() {
        assert_eq!(OffsetBase::TiffHeader.origin(100), Some(0));
        assert_eq!(OffsetBase::MakerNote { at: 0 }.origin(100), Some(100));
        assert_eq!(OffsetBase::MakerNote { at: 10 }.origin(100), Some(110));
        assert_eq!(OffsetBase::MakerNote { at: 1 }.origin(u64::MAX), None);
    }

    #[test]
    fn maker_note_layout_reads_its_own_bytes_and_vendor() {
        let maker = MakerNote {
            vendor: V::Fujifilm,
            bytes: note(b"FUJIFILM\x0c\0\0\0"),
        };
        let want = MakerNoteLayout::new(D::Fujifilm, LE, 12, RELATIVE);
        assert_eq!(maker.layout(BE), Some(want));
    }
}
