//! C2PA constants every container reader shares.
//!
//! gamut locates C2PA manifest stores in AVIF, HEIC, PNG, TIFF/DNG and WebP files and never
//! validates them. The sentence a report carries beside a located store, and the shortest value
//! that can be a store at all, are defined once here so every container reports and bounds alike;
//! each format crate re-exports them under its own `C2PA_NOT_VALIDATED` / `MIN_STORE_LEN` name.

/// What a report of a located manifest store has to say beside it, because locating one is not
/// validating it.
///
/// C2PA 2.4 §15.12 puts validation on a validator; gamut holds no COSE, X.509, RFC 3161 or
/// trust-list code and never reports a validity verdict. To anyone who has seen a Content
/// Credentials badge, "C2PA: present" printed beside EXIF and ICC reads as *verified*, so each
/// container's summary carries this text in the line that reports the store, never as a footnote.
pub const NOT_VALIDATED: &str = "gamut locates a C2PA manifest store and never validates it: \
     it checks no signature, no hash binding and no trust list. Validate with c2pa-rs.";

/// The length below which a value cannot be a manifest store: a JUMBF box header, being a 4-byte
/// `LBox` followed by a 4-byte `TBox` (C2PA 2.4 §8.4.2.3's incidental description of the box
/// framing; see `references/c2pa/README.md`). A manifest store is a JUMBF superbox, so nothing
/// shorter can be one.
pub const MIN_STORE_LEN: usize = 8;
