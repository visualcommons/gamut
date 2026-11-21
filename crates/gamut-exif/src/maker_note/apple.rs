//! A read-only view of an Apple `MakerNote` (issue #976).

use gamut_ifd::{ByteOrder, Ifd, ReadAt, Value};

use super::{
    MakerNote, MakerNoteDialect, MakerNoteLayout, Placed, read_note_ifd, single, slong, text,
};
use crate::error::Result;
use crate::value::SRational;

/// `MakerNoteVersion`: an `SLONG`.
const MAKER_NOTE_VERSION: u16 = 0x0001;
/// `AEMatrix`: an `UNDEFINED` binary plist.
const AE_MATRIX: u16 = 0x0002;
/// `RunTime`: an `UNDEFINED` binary plist (a CMTime).
const RUN_TIME: u16 = 0x0003;
/// `AEStable`: an `SLONG`.
const AE_STABLE: u16 = 0x0004;
/// `AETarget`: an `SLONG`.
const AE_TARGET: u16 = 0x0005;
/// `AEAverage`: an `SLONG`.
const AE_AVERAGE: u16 = 0x0006;
/// `AFStable`: an `SLONG`.
const AF_STABLE: u16 = 0x0007;
/// `AccelerationVector`: an `SRATIONAL` array, in g.
const ACCELERATION_VECTOR: u16 = 0x0008;
/// `HDRImageType`: an `SLONG`.
const HDR_IMAGE_TYPE: u16 = 0x000a;
/// `BurstUUID`: text.
const BURST_UUID: u16 = 0x000b;
/// `FocusDistanceRange`: an `SRATIONAL` array, in metres.
const FOCUS_DISTANCE_RANGE: u16 = 0x000c;
/// `OISMode`: an `SLONG`.
const OIS_MODE: u16 = 0x000f;
/// `ContentIdentifier`: text.
const CONTENT_IDENTIFIER: u16 = 0x0011;
/// `ImageCaptureType`: an `SLONG`.
const IMAGE_CAPTURE_TYPE: u16 = 0x0014;
/// `ImageUniqueID`: text.
const IMAGE_UNIQUE_ID: u16 = 0x0015;
/// `LivePhotoVideoIndex`: an `SLONG`.
const LIVE_PHOTO_VIDEO_INDEX: u16 = 0x0017;
/// `ImageProcessingFlags`: an `SLONG`.
const IMAGE_PROCESSING_FLAGS: u16 = 0x0019;
/// `QualityHint`: text.
const QUALITY_HINT: u16 = 0x001a;
/// `LuminanceNoiseAmplitude`: an `SRATIONAL`.
const LUMINANCE_NOISE_AMPLITUDE: u16 = 0x001d;
/// `PhotosAppFeatureFlags`: an `SLONG`.
const PHOTOS_APP_FEATURE_FLAGS: u16 = 0x001f;
/// `ImageCaptureRequestID`: text.
const IMAGE_CAPTURE_REQUEST_ID: u16 = 0x0020;
/// `HDRHeadroom`: an `SRATIONAL`.
const HDR_HEADROOM: u16 = 0x0021;
/// `AFPerformance`: an `SLONG` array.
const AF_PERFORMANCE: u16 = 0x0023;
/// `SceneFlags`: an `SLONG`.
const SCENE_FLAGS: u16 = 0x0025;
/// `SignalToNoiseRatioType`: an `SLONG`.
const SIGNAL_TO_NOISE_RATIO_TYPE: u16 = 0x0026;
/// `SignalToNoiseRatio`: an `SRATIONAL`.
const SIGNAL_TO_NOISE_RATIO: u16 = 0x0027;
/// `PhotoIdentifier`: text.
const PHOTO_IDENTIFIER: u16 = 0x002b;
/// `ColorTemperature`: an `SLONG`, in kelvin.
const COLOR_TEMPERATURE: u16 = 0x002d;
/// `CameraType`: an `SLONG`.
const CAMERA_TYPE: u16 = 0x002e;
/// `FocusPosition`: an `SLONG`.
const FOCUS_POSITION: u16 = 0x002f;
/// `HDRGain`: an `SRATIONAL`.
const HDR_GAIN: u16 = 0x0030;
/// `AFMeasuredDepth`: an `SLONG`.
const AF_MEASURED_DEPTH: u16 = 0x0038;
/// `AFConfidence`: an `SLONG`.
const AF_CONFIDENCE: u16 = 0x003d;
/// `GreenGhostMitigationStatus`: an `SLONG`.
const GREEN_GHOST_MITIGATION_STATUS: u16 = 0x003f;

impl MakerNote {
    /// The block decoded as an Apple note, given `outer` and `note_at` as for
    /// [`canon`](Self::canon). Apple's value offsets count from the note's own start and its IFD
    /// follows the byte-order mark at byte 12, so a well-formed note reads completely from these
    /// bytes alone; `outer` matters only for a note whose mark is neither `II` nor `MM`, and
    /// `note_at` only places the bytes in the stream's offset space.
    ///
    /// A value whose offset points outside the note is unreadable and named by
    /// [`AppleMakerNote::unreadable_tags`].
    ///
    /// `Ok(None)` when the block has no [`MakerNoteDialect::Apple`] layout.
    ///
    /// # Errors
    ///
    /// [`ExifError::Core`](crate::ExifError::Core) when the note's IFD itself — its entry count,
    /// entries and next-IFD pointer — does not fit in the note.
    pub fn apple(&self, outer: ByteOrder, note_at: u64) -> Result<Option<AppleMakerNote>> {
        let Some(layout) = self.layout(outer) else {
            return Ok(None);
        };
        let placed = Placed {
            bytes: &self.bytes,
            at: note_at,
        };
        AppleMakerNote::read(placed, note_at, layout)
    }
}

/// The decoded IFD of an Apple `MakerNote`: every entry, plus typed accessors for the tags staked
/// in `references/exif/apple/README.md` (issue #976).
///
/// An Apple note opens with `Apple iOS\0`, two version bytes and a byte-order mark at byte 12; its
/// IFD starts at byte 14 in the mark's order, ends in a next-IFD pointer, and counts its value
/// offsets from the note's own start ([`MakerNoteDialect::Apple`]). The view decodes that IFD
/// through [`gamut_ifd`] at the layout [`MakerNoteLayout::detect`] resolves, keeps every entry as
/// a [`Value`] ([`AppleMakerNote::ifd`]), and adds typed accessors for the tags ExifTool's
/// `Apple.pm` names. exiv2 has no Apple module, so the oracle is ExifTool's reading of the corpus
/// note, transcribed into that README.
///
/// Values are returned raw: `camera_type` is the number ExifTool prints as a camera name, the
/// arrays (`acceleration_vector`, `focus_distance_range`, `af_performance`) come back whole, and
/// the binary plists (`ae_matrix`, `run_time`) come back as their undecoded bytes.
///
/// The view never writes. The block it was read from re-emits byte-identically, because nothing
/// here changes it; its note-relative offsets stay valid wherever the writer places it (see
/// [`MakerNote`]).
///
/// Built by [`MakerNote::apple`] from the note's own bytes, or by [`read`](Self::read) from the
/// whole TIFF stream.
#[derive(Debug, Clone, PartialEq)]
pub struct AppleMakerNote {
    ifd: Ifd,
    unreadable: Vec<u16>,
}

impl AppleMakerNote {
    /// Decodes the Apple note laid out as `layout` that starts at `note_at` in `stream` — the TIFF
    /// stream the note sits in (the bytes after any `Exif\0\0` marker), or any [`ReadAt`] in that
    /// stream's offset space.
    ///
    /// `Ok(None)` when `layout` is not [`MakerNoteDialect::Apple`]. An entry whose value cannot be
    /// read is left out of [`ifd`](Self::ifd) and named by
    /// [`unreadable_tags`](Self::unreadable_tags).
    ///
    /// # Errors
    ///
    /// [`ExifError::Core`](crate::ExifError::Core) when the note's IFD — entry count, entries and
    /// next-IFD pointer — does not fit in `stream`, or `stream` fails;
    /// [`ExifError::BadOffset`](crate::ExifError::BadOffset) when `note_at` plus the IFD's offset
    /// overflows.
    pub fn read<S: ReadAt>(
        stream: S,
        note_at: u64,
        layout: MakerNoteLayout,
    ) -> Result<Option<Self>> {
        if layout.dialect != MakerNoteDialect::Apple {
            return Ok(None);
        }
        let (ifd, unreadable) = read_note_ifd(stream, note_at, layout)?;
        Ok(Some(Self { ifd, unreadable }))
    }

    /// Every entry of the note's IFD that could be read, by tag.
    #[must_use]
    pub fn ifd(&self) -> &Ifd {
        &self.ifd
    }

    /// The tags of the entries whose values could not be read (an offset past the source, or a
    /// malformed value), in on-disk order. Empty for a well-formed note.
    #[must_use]
    pub fn unreadable_tags(&self) -> &[u16] {
        &self.unreadable
    }

    /// `MakerNoteVersion` (`0x0001`).
    #[must_use]
    pub fn maker_note_version(&self) -> Option<i32> {
        slong(&self.ifd, MAKER_NOTE_VERSION)
    }

    /// `AEMatrix` (`0x0002`): the whole binary plist, undecoded.
    #[must_use]
    pub fn ae_matrix(&self) -> Option<&[u8]> {
        self.undefined(AE_MATRIX)
    }

    /// `RunTime` (`0x0003`): the whole binary plist, undecoded — a CMTime giving how long the
    /// phone has run since boot, standby excluded.
    #[must_use]
    pub fn run_time(&self) -> Option<&[u8]> {
        self.undefined(RUN_TIME)
    }

    /// `AEStable` (`0x0004`): 1 when auto-exposure had settled, 0 when not.
    #[must_use]
    pub fn ae_stable(&self) -> Option<i32> {
        slong(&self.ifd, AE_STABLE)
    }

    /// `AETarget` (`0x0005`).
    #[must_use]
    pub fn ae_target(&self) -> Option<i32> {
        slong(&self.ifd, AE_TARGET)
    }

    /// `AEAverage` (`0x0006`).
    #[must_use]
    pub fn ae_average(&self) -> Option<i32> {
        slong(&self.ifd, AE_AVERAGE)
    }

    /// `AFStable` (`0x0007`): 1 when autofocus had settled, 0 when not.
    #[must_use]
    pub fn af_stable(&self) -> Option<i32> {
        slong(&self.ifd, AF_STABLE)
    }

    /// `AccelerationVector` (`0x0008`): the whole `SRATIONAL` array, in g — X, Y and Z as
    /// ExifTool orients them (X toward the phone's left, Y toward its bottom, Z into its face).
    #[must_use]
    pub fn acceleration_vector(&self) -> Option<&[(i32, i32)]> {
        self.srationals(ACCELERATION_VECTOR)
    }

    /// `HDRImageType` (`0x000a`): Apple's HDR code (3 an HDR image, 4 the original).
    #[must_use]
    pub fn hdr_image_type(&self) -> Option<i32> {
        slong(&self.ifd, HDR_IMAGE_TYPE)
    }

    /// `BurstUUID` (`0x000b`): shared by every image of one burst.
    #[must_use]
    pub fn burst_uuid(&self) -> Option<&str> {
        text(&self.ifd, BURST_UUID)
    }

    /// `FocusDistanceRange` (`0x000c`): the whole `SRATIONAL` array, in metres, unordered.
    #[must_use]
    pub fn focus_distance_range(&self) -> Option<&[(i32, i32)]> {
        self.srationals(FOCUS_DISTANCE_RANGE)
    }

    /// `OISMode` (`0x000f`).
    #[must_use]
    pub fn ois_mode(&self) -> Option<i32> {
        slong(&self.ifd, OIS_MODE)
    }

    /// `ContentIdentifier` (`0x0011`): links a Live Photo still to its video.
    #[must_use]
    pub fn content_identifier(&self) -> Option<&str> {
        text(&self.ifd, CONTENT_IDENTIFIER)
    }

    /// `ImageCaptureType` (`0x0014`): Apple's capture-type code (1 ProRAW, 2 Portrait, 10 Photo).
    #[must_use]
    pub fn image_capture_type(&self) -> Option<i32> {
        slong(&self.ifd, IMAGE_CAPTURE_TYPE)
    }

    /// `ImageUniqueID` (`0x0015`).
    #[must_use]
    pub fn image_unique_id(&self) -> Option<&str> {
        text(&self.ifd, IMAGE_UNIQUE_ID)
    }

    /// `LivePhotoVideoIndex` (`0x0017`), as stored: one `SLONG`, the type `Apple.pm` leaves open.
    #[must_use]
    pub fn live_photo_video_index(&self) -> Option<i32> {
        slong(&self.ifd, LIVE_PHOTO_VIDEO_INDEX)
    }

    /// `ImageProcessingFlags` (`0x0019`): a bit mask.
    #[must_use]
    pub fn image_processing_flags(&self) -> Option<i32> {
        slong(&self.ifd, IMAGE_PROCESSING_FLAGS)
    }

    /// `QualityHint` (`0x001a`), e.g. `"q900n"`.
    #[must_use]
    pub fn quality_hint(&self) -> Option<&str> {
        text(&self.ifd, QUALITY_HINT)
    }

    /// `LuminanceNoiseAmplitude` (`0x001d`), the `SRATIONAL` exactly as stored.
    #[must_use]
    pub fn luminance_noise_amplitude(&self) -> Option<SRational> {
        self.srational(LUMINANCE_NOISE_AMPLITUDE)
    }

    /// `PhotosAppFeatureFlags` (`0x001f`): set when a person or pet was detected.
    #[must_use]
    pub fn photos_app_feature_flags(&self) -> Option<i32> {
        slong(&self.ifd, PHOTOS_APP_FEATURE_FLAGS)
    }

    /// `ImageCaptureRequestID` (`0x0020`).
    #[must_use]
    pub fn image_capture_request_id(&self) -> Option<&str> {
        text(&self.ifd, IMAGE_CAPTURE_REQUEST_ID)
    }

    /// `HDRHeadroom` (`0x0021`), the `SRATIONAL` exactly as stored.
    #[must_use]
    pub fn hdr_headroom(&self) -> Option<SRational> {
        self.srational(HDR_HEADROOM)
    }

    /// `AFPerformance` (`0x0023`): the whole `SLONG` array, unsplit (ExifTool prints the second
    /// value's top four bits and low 28 bits apart).
    #[must_use]
    pub fn af_performance(&self) -> Option<&[i32]> {
        match self.ifd.get(AF_PERFORMANCE)? {
            Value::SLong(values) => Some(values),
            _ => None,
        }
    }

    /// `SceneFlags` (`0x0025`): a bit mask.
    #[must_use]
    pub fn scene_flags(&self) -> Option<i32> {
        slong(&self.ifd, SCENE_FLAGS)
    }

    /// `SignalToNoiseRatioType` (`0x0026`).
    #[must_use]
    pub fn signal_to_noise_ratio_type(&self) -> Option<i32> {
        slong(&self.ifd, SIGNAL_TO_NOISE_RATIO_TYPE)
    }

    /// `SignalToNoiseRatio` (`0x0027`), the `SRATIONAL` exactly as stored.
    #[must_use]
    pub fn signal_to_noise_ratio(&self) -> Option<SRational> {
        self.srational(SIGNAL_TO_NOISE_RATIO)
    }

    /// `PhotoIdentifier` (`0x002b`).
    #[must_use]
    pub fn photo_identifier(&self) -> Option<&str> {
        text(&self.ifd, PHOTO_IDENTIFIER)
    }

    /// `ColorTemperature` (`0x002d`), in kelvin.
    #[must_use]
    pub fn color_temperature(&self) -> Option<i32> {
        slong(&self.ifd, COLOR_TEMPERATURE)
    }

    /// `CameraType` (`0x002e`): Apple's camera code (0 back wide angle, 1 back normal, 6 front).
    #[must_use]
    pub fn camera_type(&self) -> Option<i32> {
        slong(&self.ifd, CAMERA_TYPE)
    }

    /// `FocusPosition` (`0x002f`).
    #[must_use]
    pub fn focus_position(&self) -> Option<i32> {
        slong(&self.ifd, FOCUS_POSITION)
    }

    /// `HDRGain` (`0x0030`), the `SRATIONAL` exactly as stored.
    #[must_use]
    pub fn hdr_gain(&self) -> Option<SRational> {
        self.srational(HDR_GAIN)
    }

    /// `AFMeasuredDepth` (`0x0038`): from the time-of-flight-assisted autofocus estimator.
    #[must_use]
    pub fn af_measured_depth(&self) -> Option<i32> {
        slong(&self.ifd, AF_MEASURED_DEPTH)
    }

    /// `AFConfidence` (`0x003d`): that estimator's confidence.
    #[must_use]
    pub fn af_confidence(&self) -> Option<i32> {
        slong(&self.ifd, AF_CONFIDENCE)
    }

    /// `GreenGhostMitigationStatus` (`0x003f`).
    #[must_use]
    pub fn green_ghost_mitigation_status(&self) -> Option<i32> {
        slong(&self.ifd, GREEN_GHOST_MITIGATION_STATUS)
    }

    /// `tag` as a whole `UNDEFINED` value, or `None` if absent or of another type.
    fn undefined(&self, tag: u16) -> Option<&[u8]> {
        match self.ifd.get(tag)? {
            Value::Undefined(values) => Some(values),
            _ => None,
        }
    }

    /// `tag` as a whole `SRATIONAL` array, or `None` if absent or of another type.
    fn srationals(&self, tag: u16) -> Option<&[(i32, i32)]> {
        match self.ifd.get(tag)? {
            Value::SRational(values) => Some(values),
            _ => None,
        }
    }

    /// `tag` as a single `SRATIONAL`, or `None` if absent, of another type, or not one value.
    fn srational(&self, tag: u16) -> Option<SRational> {
        single(self.srationals(tag)?).map(SRational::from)
    }
}

#[cfg(test)]
mod tests {
    use gamut_ifd::ByteOrder::{BigEndian as BE, LittleEndian as LE};
    use gamut_ifd::FieldType;

    use super::super::MakerNoteVendor;
    use super::*;

    /// One IFD entry to lay out: tag, field type, count, and the value's bytes in the note's order.
    struct Entry {
        tag: u16,
        ty: FieldType,
        count: u32,
        bytes: Vec<u8>,
    }

    fn entry(tag: u16, ty: FieldType, count: usize, bytes: Vec<u8>) -> Entry {
        Entry {
            tag,
            ty,
            count: count as u32,
            bytes,
        }
    }

    fn slongs(tag: u16, values: &[i32], order: ByteOrder) -> Entry {
        let bytes = values
            .iter()
            .flat_map(|&v| order.pack_u32(v as u32))
            .collect();
        entry(tag, FieldType::SLong, values.len(), bytes)
    }

    fn srationals(tag: u16, values: &[(i32, i32)], order: ByteOrder) -> Entry {
        let bytes = values
            .iter()
            .flat_map(|&(n, d)| [order.pack_u32(n as u32), order.pack_u32(d as u32)])
            .flatten()
            .collect();
        entry(tag, FieldType::SRational, values.len(), bytes)
    }

    fn undefined(tag: u16, values: &[u8]) -> Entry {
        entry(tag, FieldType::Undefined, values.len(), values.to_vec())
    }

    fn ascii(tag: u16, text: &[u8]) -> Entry {
        entry(tag, FieldType::Ascii, text.len(), text.to_vec())
    }

    /// An Apple note in `order`: the 14-byte header naming that order, the IFD, a zero next-IFD
    /// pointer, then each out-of-line value, addressed from the note's first byte.
    fn apple_note(entries: &[Entry], order: ByteOrder) -> Vec<u8> {
        let mut note = b"Apple iOS\0\0\x01".to_vec();
        note.extend_from_slice(match order {
            LE => b"II",
            BE => b"MM",
        });
        let count = entries.len() as u32;
        let mut data_at = 14 + 2 + 12 * count + 4;
        let mut data = Vec::new();
        note.extend_from_slice(&order.pack_u16(count as u16));
        for entry in entries {
            note.extend_from_slice(&order.pack_u16(entry.tag));
            note.extend_from_slice(&order.pack_u16(entry.ty.code()));
            note.extend_from_slice(&order.pack_u32(entry.count));
            if entry.bytes.len() <= 4 {
                let mut word = entry.bytes.clone();
                word.resize(4, 0);
                note.extend_from_slice(&word);
            } else {
                note.extend_from_slice(&order.pack_u32(data_at));
                data.extend_from_slice(&entry.bytes);
                data_at += entry.bytes.len() as u32;
            }
        }
        note.extend_from_slice(&[0; 4]);
        note.extend_from_slice(&data);
        note
    }

    fn maker(bytes: Vec<u8>) -> MakerNote {
        MakerNote {
            vendor: MakerNoteVendor::Apple,
            bytes,
        }
    }

    /// One entry per accessor, each holding a value no other accessor's tag holds, so reading the
    /// wrong tag is always visible.
    fn every_tag(order: ByteOrder) -> Vec<Entry> {
        let s = |tag, value| slongs(tag, &[value], order);
        vec![
            s(MAKER_NOTE_VERSION, 12),
            undefined(AE_MATRIX, b"bplist00 matrix"),
            undefined(RUN_TIME, b"bplist00 time"),
            s(AE_STABLE, 2),
            s(AE_TARGET, 186),
            s(AE_AVERAGE, 180),
            s(AF_STABLE, 3),
            srationals(
                ACCELERATION_VECTOR,
                &[(-17, 41), (-61, 60), (-4, 45)],
                order,
            ),
            s(HDR_IMAGE_TYPE, 4),
            ascii(BURST_UUID, b"BURST-1\0"),
            srationals(FOCUS_DISTANCE_RANGE, &[(67, 256), (97, 256)], order),
            s(OIS_MODE, 5),
            ascii(CONTENT_IDENTIFIER, b"CONTENT-2\0"),
            s(IMAGE_CAPTURE_TYPE, 10),
            ascii(IMAGE_UNIQUE_ID, b"UNIQUE-3\0"),
            s(LIVE_PHOTO_VIDEO_INDEX, 1_112_547_584),
            s(IMAGE_PROCESSING_FLAGS, 65_536),
            ascii(QUALITY_HINT, b"q900n\0"),
            srationals(LUMINANCE_NOISE_AMPLITUDE, &[(-7, 9)], order),
            s(PHOTOS_APP_FEATURE_FLAGS, 6),
            ascii(IMAGE_CAPTURE_REQUEST_ID, b"REQUEST-4\0"),
            srationals(HDR_HEADROOM, &[(63_809, 37_837)], order),
            slongs(AF_PERFORMANCE, &[954, 268_435_495], order),
            s(SCENE_FLAGS, 134),
            s(SIGNAL_TO_NOISE_RATIO_TYPE, 7),
            srationals(SIGNAL_TO_NOISE_RATIO, &[(466_861, 7_493)], order),
            ascii(PHOTO_IDENTIFIER, b"PHOTO-5\0"),
            s(COLOR_TEMPERATURE, 5_358),
            s(CAMERA_TYPE, 8),
            s(FOCUS_POSITION, 55),
            srationals(HDR_GAIN, &[(3, 11)], order),
            s(AF_MEASURED_DEPTH, 403),
            s(AF_CONFIDENCE, 9),
            s(GREEN_GHOST_MITIGATION_STATUS, -13),
        ]
    }

    #[test]
    fn every_accessor_reads_its_own_tag() {
        for order in [LE, BE] {
            let note = maker(apple_note(&every_tag(order), order));
            // The mark decides the order, so the outer stream's never matters.
            let view = note.apple(BE, 200).expect("reads").expect("is Apple");
            assert_eq!(view.maker_note_version(), Some(12));
            assert_eq!(view.ae_matrix(), Some(&b"bplist00 matrix"[..]));
            assert_eq!(view.run_time(), Some(&b"bplist00 time"[..]));
            assert_eq!(view.ae_stable(), Some(2));
            assert_eq!(view.ae_target(), Some(186));
            assert_eq!(view.ae_average(), Some(180));
            assert_eq!(view.af_stable(), Some(3));
            assert_eq!(
                view.acceleration_vector(),
                Some(&[(-17, 41), (-61, 60), (-4, 45)][..])
            );
            assert_eq!(view.hdr_image_type(), Some(4));
            assert_eq!(view.burst_uuid(), Some("BURST-1"));
            assert_eq!(
                view.focus_distance_range(),
                Some(&[(67, 256), (97, 256)][..])
            );
            assert_eq!(view.ois_mode(), Some(5));
            assert_eq!(view.content_identifier(), Some("CONTENT-2"));
            assert_eq!(view.image_capture_type(), Some(10));
            assert_eq!(view.image_unique_id(), Some("UNIQUE-3"));
            assert_eq!(view.live_photo_video_index(), Some(1_112_547_584));
            assert_eq!(view.image_processing_flags(), Some(65_536));
            assert_eq!(view.quality_hint(), Some("q900n"));
            assert_eq!(
                view.luminance_noise_amplitude(),
                Some(SRational::from((-7, 9)))
            );
            assert_eq!(view.photos_app_feature_flags(), Some(6));
            assert_eq!(view.image_capture_request_id(), Some("REQUEST-4"));
            assert_eq!(view.hdr_headroom(), Some(SRational::from((63_809, 37_837))));
            assert_eq!(view.af_performance(), Some(&[954, 268_435_495][..]));
            assert_eq!(view.scene_flags(), Some(134));
            assert_eq!(view.signal_to_noise_ratio_type(), Some(7));
            assert_eq!(
                view.signal_to_noise_ratio(),
                Some(SRational::from((466_861, 7_493)))
            );
            assert_eq!(view.photo_identifier(), Some("PHOTO-5"));
            assert_eq!(view.color_temperature(), Some(5_358));
            assert_eq!(view.camera_type(), Some(8));
            assert_eq!(view.focus_position(), Some(55));
            assert_eq!(view.hdr_gain(), Some(SRational::from((3, 11))));
            assert_eq!(view.af_measured_depth(), Some(403));
            assert_eq!(view.af_confidence(), Some(9));
            assert_eq!(view.green_ghost_mitigation_status(), Some(-13));
            assert_eq!(view.ifd().fields().len(), 34);
            assert!(view.unreadable_tags().is_empty());
        }
    }

    /// Each typed accessor shape refuses the entry under another type, and the raw value stays
    /// reachable through `ifd()`.
    #[test]
    fn a_retyped_tag_reads_as_none() {
        let order = LE;
        let note = maker(apple_note(
            &[
                ascii(AE_MATRIX, b"bplist\0"),
                slongs(ACCELERATION_VECTOR, &[1, 2, 3], order),
                slongs(HDR_HEADROOM, &[1], order),
                srationals(AF_PERFORMANCE, &[(1, 2)], order),
                undefined(PHOTO_IDENTIFIER, b"PHOTO"),
                srationals(CAMERA_TYPE, &[(1, 1)], order),
            ],
            order,
        ));
        let view = note.apple(order, 0).expect("reads").expect("is Apple");
        assert_eq!(view.ae_matrix(), None);
        assert_eq!(view.acceleration_vector(), None);
        assert_eq!(view.hdr_headroom(), None);
        assert_eq!(view.af_performance(), None);
        assert_eq!(view.photo_identifier(), None);
        assert_eq!(view.camera_type(), None);
        assert_eq!(view.ifd().get(HDR_HEADROOM), Some(&Value::SLong(vec![1])));
    }

    #[test]
    fn an_absent_tag_reads_as_none() {
        let order = BE;
        let note = maker(apple_note(&[slongs(OIS_MODE, &[2], order)], order));
        let view = note.apple(order, 0).expect("reads").expect("is Apple");
        assert_eq!(view.run_time(), None);
        assert_eq!(view.focus_distance_range(), None);
        assert_eq!(view.hdr_gain(), None);
        assert_eq!(view.af_performance(), None);
        assert_eq!(view.burst_uuid(), None);
        assert_eq!(view.maker_note_version(), None);
    }

    /// A scalar accessor takes exactly one value; ExifTool would print them all.
    #[test]
    fn a_scalar_with_more_than_one_value_reads_as_none() {
        let order = LE;
        let note = maker(apple_note(
            &[
                srationals(SIGNAL_TO_NOISE_RATIO, &[(1, 3), (2, 3)], order),
                slongs(FOCUS_POSITION, &[1, 2], order),
            ],
            order,
        ));
        let view = note.apple(order, 0).expect("reads").expect("is Apple");
        assert_eq!(view.signal_to_noise_ratio(), None);
        assert_eq!(view.focus_position(), None);
    }

    /// Note-relative offsets read the same wherever the note stands, from its own bytes or through
    /// the stream around it.
    #[test]
    fn offsets_count_from_the_note_start() {
        let order = BE;
        let bytes = apple_note(&every_tag(order), order);
        let here = maker(bytes.clone()).apple(LE, 0).expect("reads");
        assert_eq!(here, maker(bytes.clone()).apple(LE, 4_096).expect("reads"));
        let mut stream = vec![0; 300];
        stream.extend_from_slice(&bytes);
        let layout = maker(bytes).layout(LE).expect("Apple layout");
        let read = AppleMakerNote::read(&stream[..], 300, layout).expect("reads");
        assert_eq!(read, here);
    }

    #[test]
    fn a_value_past_the_note_is_unreadable_and_named() {
        let order = LE;
        let mut bytes = apple_note(&[ascii(QUALITY_HINT, b"q900n\0")], order);
        bytes.truncate(bytes.len() - 1);
        let view = maker(bytes)
            .apple(order, 0)
            .expect("reads")
            .expect("is Apple");
        assert_eq!(view.quality_hint(), None);
        assert_eq!(view.unreadable_tags(), &[QUALITY_HINT]);
    }

    #[test]
    fn a_note_without_its_next_ifd_pointer_is_an_error() {
        let order = LE;
        let mut bytes = apple_note(&[slongs(OIS_MODE, &[1], order)], order);
        bytes.truncate(14 + 14);
        assert!(maker(bytes).apple(order, 0).is_err());
    }

    #[test]
    fn only_an_apple_layout_is_read() {
        let order = LE;
        let bytes = apple_note(&[slongs(OIS_MODE, &[1], order)], order);
        let canon = MakerNote {
            vendor: MakerNoteVendor::Canon,
            bytes: bytes.clone(),
        };
        assert_eq!(canon.apple(order, 0).expect("not an error"), None);
        let unsigned = maker(bytes[14..].to_vec());
        assert_eq!(unsigned.apple(order, 0).expect("not an error"), None);
    }
}
