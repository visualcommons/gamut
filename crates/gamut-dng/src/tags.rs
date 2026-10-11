//! DNG tag numbers — the 2-byte `Tag` field of an IFD entry.
//!
//! Values are taken from the **DNG 1.7.1.0** specification and the Adobe DNG SDK's
//! `dng_tag_codes.h` (the `tc*` enum). They fall into three groups: the baseline TIFF 6.0 / TIFF-EP
//! tags DNG inherits, the DNG-private tags (`0xC612`–`0xC7B5`), and the DNG 1.6/1.7 additions
//! (`0xCD2D`–`0xCD4D`). Only the tags the encoder/decoder act on are named; an unknown tag is still
//! parsed structurally by [`gamut_ifd`].
//!
//! Hex appears in each doc comment because Adobe and most DNG tooling refer to these tags by hex.

// ---------------------------------------------------------------------------------------------
// Baseline TIFF 6.0 / TIFF-EP tags that DNG uses.
// ---------------------------------------------------------------------------------------------

/// `NewSubFileType` (254, 0x00FE) — a bit field describing the kind of data in this subfile
/// (see [`crate::values::new_subfile_type`]).
pub const NEW_SUBFILE_TYPE: u16 = 254;
/// `ImageWidth` (256, 0x0100) — the number of columns (pixels per row).
pub const IMAGE_WIDTH: u16 = 256;
/// `ImageLength` (257, 0x0101) — the number of rows (scanlines).
pub const IMAGE_LENGTH: u16 = 257;
/// `BitsPerSample` (258, 0x0102) — bits per component, one value per sample.
pub const BITS_PER_SAMPLE: u16 = 258;
/// `Compression` (259, 0x0103) — the compression scheme applied to the image data.
pub const COMPRESSION: u16 = 259;
/// `PhotometricInterpretation` (262, 0x0106) — the colour space / raw photometry of the data.
pub const PHOTOMETRIC_INTERPRETATION: u16 = 262;
/// `ImageDescription` (270, 0x010E) — an ASCII description of the image.
pub const IMAGE_DESCRIPTION: u16 = 270;
/// `Make` (271, 0x010F) — the camera manufacturer.
pub const MAKE: u16 = 271;
/// `Model` (272, 0x0110) — the camera model.
pub const MODEL: u16 = 272;
/// `StripOffsets` (273, 0x0111) — the byte offset of each strip.
pub const STRIP_OFFSETS: u16 = 273;
/// `Orientation` (274, 0x0112) — the image orientation; DNG readers support all 8 values.
pub const ORIENTATION: u16 = 274;
/// `SamplesPerPixel` (277, 0x0115) — the number of components per pixel (colour planes).
pub const SAMPLES_PER_PIXEL: u16 = 277;
/// `RowsPerStrip` (278, 0x0116) — the number of rows in each strip.
pub const ROWS_PER_STRIP: u16 = 278;
/// `StripByteCounts` (279, 0x0117) — the number of (compressed) bytes in each strip.
pub const STRIP_BYTE_COUNTS: u16 = 279;
/// `XResolution` (282, 0x011A) — pixels per resolution unit, horizontal.
pub const X_RESOLUTION: u16 = 282;
/// `YResolution` (283, 0x011B) — pixels per resolution unit, vertical.
pub const Y_RESOLUTION: u16 = 283;
/// `PlanarConfiguration` (284, 0x011C) — chunky (1) or planar (2) component storage.
pub const PLANAR_CONFIGURATION: u16 = 284;
/// `Predictor` (317, 0x013D) — the differencing transform applied before compression.
pub const PREDICTOR: u16 = 317;
/// `ResolutionUnit` (296, 0x0128) — the unit for `XResolution`/`YResolution`.
pub const RESOLUTION_UNIT: u16 = 296;
/// `Software` (305, 0x0131) — the name/version of the writing software.
pub const SOFTWARE: u16 = 305;
/// `DateTime` (306, 0x0132) — file change date and time (`YYYY:MM:DD HH:MM:SS`).
pub const DATE_TIME: u16 = 306;
/// `Artist` (315, 0x013B) — the person who created the image.
pub const ARTIST: u16 = 315;
/// `TileWidth` (322, 0x0142) — the width of each tile in pixels.
pub const TILE_WIDTH: u16 = 322;
/// `TileLength` (323, 0x0143) — the height of each tile in pixels.
pub const TILE_LENGTH: u16 = 323;
/// `TileOffsets` (324, 0x0144) — the byte offset of each tile.
pub const TILE_OFFSETS: u16 = 324;
/// `TileByteCounts` (325, 0x0145) — the number of (compressed) bytes in each tile.
pub const TILE_BYTE_COUNTS: u16 = 325;
/// `SubIFDs` (330, 0x014A) — offsets of child IFDs (DNG points this at the raw image IFD(s)).
pub const SUB_IFDS: u16 = gamut_ifd::tags::SUB_IFDS;
/// `ExtraSamples` (338, 0x0152) — the meaning of each component beyond the photometric ones.
pub const EXTRA_SAMPLES: u16 = 338;
/// `SampleFormat` (339, 0x0153) — how each sample is encoded (unsigned/signed int, float).
pub const SAMPLE_FORMAT: u16 = 339;
/// `JPEGTables` (347, 0x015B) — shared JPEG quantization/Huffman tables for JPEG-compressed tiles.
pub const JPEG_TABLES: u16 = 347;
/// `XMP` (700, 0x02BC) — an embedded XMP packet (UTF-8 RDF/XML), stored as `BYTE`.
pub const XMP: u16 = 700;
/// `CFARepeatPatternDim` (33421, 0x828D) — the rows/columns of the repeating CFA pattern tile.
pub const CFA_REPEAT_PATTERN_DIM: u16 = 33421;
/// `CFAPattern` (33422, 0x828E) — the colour of each sensel in the repeating CFA tile.
pub const CFA_PATTERN: u16 = 33422;
/// `Copyright` (33432, 0x8298) — copyright notice.
pub const COPYRIGHT: u16 = 33432;
/// `IPTC/NAA` (33723, 0x83BB) — embedded IPTC-IIM metadata.
pub const IPTC_NAA: u16 = 33723;
/// `ExifIFD` (34665, 0x8769) — offset of the EXIF sub-IFD.
pub const EXIF_IFD: u16 = gamut_ifd::tags::EXIF_IFD;
/// `ICCProfile` (34675, 0x8773) — an embedded ICC colour profile.
pub const ICC_PROFILE: u16 = 34675;
/// `GPSInfo` (34853, 0x8825) — offset of the GPS sub-IFD.
pub const GPS_INFO: u16 = gamut_ifd::tags::GPS_INFO;

// ---------------------------------------------------------------------------------------------
// EXIF sub-IFD tags (a constrained TIFF IFD reached through `ExifIFD`).
// ---------------------------------------------------------------------------------------------

/// `ExposureTime` (33434, 0x829A) — exposure time in seconds (`RATIONAL`).
pub const EXPOSURE_TIME: u16 = 33434;
/// `FNumber` (33437, 0x829D) — the lens F number (`RATIONAL`).
pub const F_NUMBER: u16 = 33437;
/// `ISOSpeedRatings` (34855, 0x8827) — the ISO speed (`SHORT`).
pub const ISO_SPEED_RATINGS: u16 = 34855;
/// `ExifVersion` (36864, 0x9000) — the EXIF version, four ASCII bytes (`UNDEFINED`).
pub const EXIF_VERSION: u16 = 36864;
/// `DateTimeOriginal` (36867, 0x9003) — capture date/time (`ASCII`, `YYYY:MM:DD HH:MM:SS`).
pub const DATE_TIME_ORIGINAL: u16 = 36867;
/// `FocalLength` (37386, 0x920A) — lens focal length in mm (`RATIONAL`).
pub const FOCAL_LENGTH: u16 = 37386;

// ---------------------------------------------------------------------------------------------
// DNG-private tags (0xC612–0xC7B5).
// ---------------------------------------------------------------------------------------------

/// `DNGVersion` (50706, 0xC612) — four bytes: the DNG spec version the file conforms to.
pub const DNG_VERSION: u16 = 50706;
/// `DNGBackwardVersion` (50707, 0xC613) — the oldest DNG version a reader needs to fully parse it.
pub const DNG_BACKWARD_VERSION: u16 = 50707;
/// `UniqueCameraModel` (50708, 0xC614) — a non-localized, unique camera model name.
pub const UNIQUE_CAMERA_MODEL: u16 = 50708;
/// `LocalizedCameraModel` (50709, 0xC615) — a localized camera model name.
pub const LOCALIZED_CAMERA_MODEL: u16 = 50709;
/// `CFAPlaneColor` (50710, 0xC616) — maps each CFA plane index to a colour.
pub const CFA_PLANE_COLOR: u16 = 50710;
/// `CFALayout` (50711, 0xC617) — the physical layout of the CFA (rectangular / staggered).
pub const CFA_LAYOUT: u16 = 50711;
/// `LinearizationTable` (50712, 0xC618) — a lookup table mapping stored values to linear values.
pub const LINEARIZATION_TABLE: u16 = 50712;
/// `BlackLevelRepeatDim` (50713, 0xC619) — the rows/columns of the repeating black-level pattern.
pub const BLACK_LEVEL_REPEAT_DIM: u16 = 50713;
/// `BlackLevel` (50714, 0xC61A) — the zero-light encoding level, per repeat-pattern position/plane.
pub const BLACK_LEVEL: u16 = 50714;
/// `BlackLevelDeltaH` (50715, 0xC61B) — a per-column black-level adjustment.
pub const BLACK_LEVEL_DELTA_H: u16 = 50715;
/// `BlackLevelDeltaV` (50716, 0xC61C) — a per-row black-level adjustment.
pub const BLACK_LEVEL_DELTA_V: u16 = 50716;
/// `WhiteLevel` (50717, 0xC61D) — the fully-saturated encoding level, per plane.
pub const WHITE_LEVEL: u16 = 50717;
/// `DefaultScale` (50718, 0xC61E) — the default scaling factors (for non-square pixels).
pub const DEFAULT_SCALE: u16 = 50718;
/// `DefaultCropOrigin` (50719, 0xC61F) — the origin of the default crop rectangle.
pub const DEFAULT_CROP_ORIGIN: u16 = 50719;
/// `DefaultCropSize` (50720, 0xC620) — the size of the default crop rectangle.
pub const DEFAULT_CROP_SIZE: u16 = 50720;
/// `ColorMatrix1` (50721, 0xC621) — XYZ → reference-camera-native colour matrix for illuminant 1.
pub const COLOR_MATRIX1: u16 = 50721;
/// `ColorMatrix2` (50722, 0xC622) — XYZ → reference-camera-native colour matrix for illuminant 2.
pub const COLOR_MATRIX2: u16 = 50722;
/// `CameraCalibration1` (50723, 0xC623) — per-camera calibration matrix for illuminant 1.
pub const CAMERA_CALIBRATION1: u16 = 50723;
/// `CameraCalibration2` (50724, 0xC624) — per-camera calibration matrix for illuminant 2.
pub const CAMERA_CALIBRATION2: u16 = 50724;
/// `ReductionMatrix1` (50725, 0xC625) — dimension-reduction matrix for illuminant 1 (>3 planes).
pub const REDUCTION_MATRIX1: u16 = 50725;
/// `ReductionMatrix2` (50726, 0xC626) — dimension-reduction matrix for illuminant 2 (>3 planes).
pub const REDUCTION_MATRIX2: u16 = 50726;
/// `AnalogBalance` (50727, 0xC627) — the gain applied to each plane before the colour matrix.
pub const ANALOG_BALANCE: u16 = 50727;
/// `AsShotNeutral` (50728, 0xC628) — the as-shot white balance as camera-native neutral coords.
pub const AS_SHOT_NEUTRAL: u16 = 50728;
/// `AsShotWhiteXY` (50729, 0xC629) — the as-shot white balance as CIE xy chromaticity.
pub const AS_SHOT_WHITE_XY: u16 = 50729;
/// `BaselineExposure` (50730, 0xC62A) — the default exposure compensation, in stops.
pub const BASELINE_EXPOSURE: u16 = 50730;
/// `BaselineNoise` (50731, 0xC62B) — the relative noise level at ISO 100.
pub const BASELINE_NOISE: u16 = 50731;
/// `BaselineSharpness` (50732, 0xC62C) — the relative amount of sharpening to apply.
pub const BASELINE_SHARPNESS: u16 = 50732;
/// `BayerGreenSplit` (50733, 0xC62D) — how much the two Bayer green channels differ.
pub const BAYER_GREEN_SPLIT: u16 = 50733;
/// `LinearResponseLimit` (50734, 0xC62E) — the fraction of the range over which response is linear.
pub const LINEAR_RESPONSE_LIMIT: u16 = 50734;
/// `CameraSerialNumber` (50735, 0xC62F) — the camera body serial number.
pub const CAMERA_SERIAL_NUMBER: u16 = 50735;
/// `LensInfo` (50736, 0xC630) — the minimum/maximum focal length and aperture of the lens.
pub const LENS_INFO: u16 = 50736;
/// `ChromaBlurRadius` (50737, 0xC631) — the chroma-blur radius to apply during demosaic.
pub const CHROMA_BLUR_RADIUS: u16 = 50737;
/// `AntiAliasStrength` (50738, 0xC632) — the relative strength of the anti-alias (OLPF) filter.
pub const ANTI_ALIAS_STRENGTH: u16 = 50738;
/// `ShadowScale` (50739, 0xC633) — a legacy Camera Raw shadow-slider scale factor.
pub const SHADOW_SCALE: u16 = 50739;
/// `DNGPrivateData` (50740, 0xC634) — opaque manufacturer-private data.
pub const DNG_PRIVATE_DATA: u16 = 50740;
/// `MakerNoteSafety` (50741, 0xC635) — whether the EXIF MakerNote is safe to copy (0 = unsafe).
pub const MAKER_NOTE_SAFETY: u16 = 50741;
/// `CalibrationIlluminant1` (50778, 0xC65A) — the EXIF LightSource for `ColorMatrix1`.
pub const CALIBRATION_ILLUMINANT1: u16 = 50778;
/// `CalibrationIlluminant2` (50779, 0xC65B) — the EXIF LightSource for `ColorMatrix2`.
pub const CALIBRATION_ILLUMINANT2: u16 = 50779;
/// `BestQualityScale` (50780, 0xC65C) — the scale factor for best-quality (non-square) rendering.
pub const BEST_QUALITY_SCALE: u16 = 50780;
/// `RawDataUniqueID` (50781, 0xC65D) — a 16-byte unique identifier for the raw image data.
pub const RAW_DATA_UNIQUE_ID: u16 = 50781;
/// `OriginalRawFileName` (50827, 0xC68B) — the file name of the original raw, if converted.
pub const ORIGINAL_RAW_FILE_NAME: u16 = 50827;
/// `OriginalRawFileData` (50828, 0xC68C) — the original raw file's data, if embedded.
pub const ORIGINAL_RAW_FILE_DATA: u16 = 50828;
/// `ActiveArea` (50829, 0xC68D) — the rectangle of pixels holding actual image data.
pub const ACTIVE_AREA: u16 = 50829;
/// `MaskedAreas` (50830, 0xC68E) — rectangles of optically-masked (black-reference) pixels.
pub const MASKED_AREAS: u16 = 50830;
/// `AsShotICCProfile` (50831, 0xC68F) — an as-shot ICC profile for the camera.
pub const AS_SHOT_ICC_PROFILE: u16 = 50831;
/// `AsShotPreProfileMatrix` (50832, 0xC690) — a matrix applied before the as-shot ICC profile.
pub const AS_SHOT_PRE_PROFILE_MATRIX: u16 = 50832;
/// `CurrentICCProfile` (50833, 0xC691) — the current ICC profile.
pub const CURRENT_ICC_PROFILE: u16 = 50833;
/// `CurrentPreProfileMatrix` (50834, 0xC692) — a matrix applied before the current ICC profile.
pub const CURRENT_PRE_PROFILE_MATRIX: u16 = 50834;
/// `ColorimetricReference` (50879, 0xC6BF) — the colorimetric reference (scene vs output referred).
pub const COLORIMETRIC_REFERENCE: u16 = 50879;
/// `CameraCalibrationSignature` (50931, 0xC6F3) — identifies who created the camera calibration.
pub const CAMERA_CALIBRATION_SIGNATURE: u16 = 50931;
/// `ProfileCalibrationSignature` (50932, 0xC6F4) — the calibration signature a profile requires.
pub const PROFILE_CALIBRATION_SIGNATURE: u16 = 50932;
/// `ExtraCameraProfiles` (50933, 0xC6F5) — offsets of additional embedded camera profiles.
pub const EXTRA_CAMERA_PROFILES: u16 = 50933;
/// `AsShotProfileName` (50934, 0xC6F6) — the name of the as-shot camera profile.
pub const AS_SHOT_PROFILE_NAME: u16 = 50934;
/// `NoiseReductionApplied` (50935, 0xC6F7) — how much noise reduction was already applied.
pub const NOISE_REDUCTION_APPLIED: u16 = 50935;
/// `ProfileName` (50936, 0xC6F8) — the name of this camera profile.
pub const PROFILE_NAME: u16 = 50936;
/// `ProfileHueSatMapDims` (50937, 0xC6F9) — the dimensions of the hue/sat/value mapping table.
pub const PROFILE_HUE_SAT_MAP_DIMS: u16 = 50937;
/// `ProfileHueSatMapData1` (50938, 0xC6FA) — the hue/sat/value mapping data for illuminant 1.
pub const PROFILE_HUE_SAT_MAP_DATA1: u16 = 50938;
/// `ProfileHueSatMapData2` (50939, 0xC6FB) — the hue/sat/value mapping data for illuminant 2.
pub const PROFILE_HUE_SAT_MAP_DATA2: u16 = 50939;
/// `ProfileToneCurve` (50940, 0xC6FC) — the default tone curve for this profile.
pub const PROFILE_TONE_CURVE: u16 = 50940;
/// `ProfileEmbedPolicy` (50941, 0xC6FD) — the embedding/usage policy for this profile.
pub const PROFILE_EMBED_POLICY: u16 = 50941;
/// `ProfileCopyright` (50942, 0xC6FE) — copyright notice for this profile.
pub const PROFILE_COPYRIGHT: u16 = 50942;
/// `ForwardMatrix1` (50964, 0xC714) — white-balanced camera-native → XYZ(D50) matrix, illuminant 1.
pub const FORWARD_MATRIX1: u16 = 50964;
/// `ForwardMatrix2` (50965, 0xC715) — white-balanced camera-native → XYZ(D50) matrix, illuminant 2.
pub const FORWARD_MATRIX2: u16 = 50965;
/// `PreviewApplicationName` (50966, 0xC716) — the app that rendered the preview.
pub const PREVIEW_APPLICATION_NAME: u16 = 50966;
/// `PreviewApplicationVersion` (50967, 0xC717) — that app's version.
pub const PREVIEW_APPLICATION_VERSION: u16 = 50967;
/// `PreviewSettingsName` (50968, 0xC718) — the name of the settings used to render the preview.
pub const PREVIEW_SETTINGS_NAME: u16 = 50968;
/// `PreviewSettingsDigest` (50969, 0xC719) — a digest of those settings.
pub const PREVIEW_SETTINGS_DIGEST: u16 = 50969;
/// `PreviewColorSpace` (50970, 0xC71A) — the colour space of the preview image.
pub const PREVIEW_COLOR_SPACE: u16 = 50970;
/// `PreviewDateTime` (50971, 0xC71B) — when the preview was rendered (ISO 8601).
pub const PREVIEW_DATE_TIME: u16 = 50971;
/// `RawImageDigest` (50972, 0xC71C) — an MD5 digest of the (compressed) raw image data.
pub const RAW_IMAGE_DIGEST: u16 = 50972;
/// `OriginalRawFileDigest` (50973, 0xC71D) — an MD5 digest of the original raw file.
pub const ORIGINAL_RAW_FILE_DIGEST: u16 = 50973;
/// `SubTileBlockSize` (50974, 0xC71E) — the sub-tile block dimensions for tiled storage.
pub const SUB_TILE_BLOCK_SIZE: u16 = 50974;
/// `RowInterleaveFactor` (50975, 0xC71F) — the row interleave factor of the raw data.
pub const ROW_INTERLEAVE_FACTOR: u16 = 50975;
/// `ProfileLookTableDims` (50981, 0xC725) — the dimensions of the profile look table.
pub const PROFILE_LOOK_TABLE_DIMS: u16 = 50981;
/// `ProfileLookTableData` (50982, 0xC726) — the profile look-table data.
pub const PROFILE_LOOK_TABLE_DATA: u16 = 50982;
/// `OpcodeList1` (51008, 0xC740) — opcodes applied to the raw image as read from the file.
pub const OPCODE_LIST1: u16 = 51008;
/// `OpcodeList2` (51009, 0xC741) — opcodes applied after mapping to linear reference values.
pub const OPCODE_LIST2: u16 = 51009;
/// `OpcodeList3` (51022, 0xC74E) — opcodes applied after demosaicing.
pub const OPCODE_LIST3: u16 = 51022;
/// `NoiseProfile` (51041, 0xC761) — the camera's noise model (per plane).
pub const NOISE_PROFILE: u16 = 51041;
/// `OriginalDefaultFinalSize` (51089, 0xC791) — the original default final image size.
pub const ORIGINAL_DEFAULT_FINAL_SIZE: u16 = 51089;
/// `OriginalBestQualityFinalSize` (51090, 0xC792) — the original best-quality final image size.
pub const ORIGINAL_BEST_QUALITY_FINAL_SIZE: u16 = 51090;
/// `OriginalDefaultCropSize` (51091, 0xC793) — the original default crop size.
pub const ORIGINAL_DEFAULT_CROP_SIZE: u16 = 51091;
/// `ProfileHueSatMapEncoding` (51107, 0xC7A3) — the colour encoding of the hue/sat map.
pub const PROFILE_HUE_SAT_MAP_ENCODING: u16 = 51107;
/// `ProfileLookTableEncoding` (51108, 0xC7A4) — the colour encoding of the look table.
pub const PROFILE_LOOK_TABLE_ENCODING: u16 = 51108;
/// `BaselineExposureOffset` (51109, 0xC7A5) — an exposure offset baked into the profile.
pub const BASELINE_EXPOSURE_OFFSET: u16 = 51109;
/// `DefaultBlackRender` (51110, 0xC7A6) — how to render blacks (auto vs none) for this profile.
pub const DEFAULT_BLACK_RENDER: u16 = 51110;
/// `NewRawImageDigest` (51111, 0xC7A7) — the current MD5 digest scheme for the raw image data.
pub const NEW_RAW_IMAGE_DIGEST: u16 = 51111;
/// `RawToPreviewGain` (51112, 0xC7A8) — the gain between the raw image and the preview.
pub const RAW_TO_PREVIEW_GAIN: u16 = 51112;
/// `DefaultUserCrop` (51125, 0xC7B5) — the default user crop rectangle (fractional).
pub const DEFAULT_USER_CROP: u16 = 51125;

// ---------------------------------------------------------------------------------------------
// Depth-map tags (0xC7E9–0xC7EE).
// ---------------------------------------------------------------------------------------------

/// `DepthFormat` (51177, 0xC7E9) — how depth values are encoded (linear/inverse/exponential).
pub const DEPTH_FORMAT: u16 = 51177;
/// `DepthNear` (51178, 0xC7EA) — the distance mapped to the minimum depth value.
pub const DEPTH_NEAR: u16 = 51178;
/// `DepthFar` (51179, 0xC7EB) — the distance mapped to the maximum depth value.
pub const DEPTH_FAR: u16 = 51179;
/// `DepthUnits` (51180, 0xC7EC) — the units of `DepthNear`/`DepthFar`.
pub const DEPTH_UNITS: u16 = 51180;
/// `DepthMeasureType` (51181, 0xC7ED) — how distance is measured (optical axis vs optical ray).
pub const DEPTH_MEASURE_TYPE: u16 = 51181;
/// `EnhanceParams` (51182, 0xC7EE) — parameters describing an enhanced (e.g. denoised) image.
pub const ENHANCE_PARAMS: u16 = 51182;

// ---------------------------------------------------------------------------------------------
// DNG 1.6 / 1.7 additions (0xCD2D–0xCD4D).
// ---------------------------------------------------------------------------------------------

/// `ProfileGainTableMap` (52525, 0xCD2D) — a spatially-varying gain map for the profile (DNG 1.6).
pub const PROFILE_GAIN_TABLE_MAP: u16 = 52525;
/// `SemanticName` (52526, 0xCD2E) — the name of a semantic mask (DNG 1.6).
pub const SEMANTIC_NAME: u16 = 52526;
/// `SemanticInstanceID` (52528, 0xCD30) — a unique id for a semantic mask instance (DNG 1.6).
pub const SEMANTIC_INSTANCE_ID: u16 = 52528;
/// `CalibrationIlluminant3` (52529, 0xCD31) — the EXIF LightSource for `ColorMatrix3` (DNG 1.6).
pub const CALIBRATION_ILLUMINANT3: u16 = 52529;
/// `CameraCalibration3` (52530, 0xCD32) — per-camera calibration matrix for illuminant 3 (DNG 1.6).
pub const CAMERA_CALIBRATION3: u16 = 52530;
/// `ColorMatrix3` (52531, 0xCD33) — XYZ → camera-native colour matrix for illuminant 3 (DNG 1.6).
pub const COLOR_MATRIX3: u16 = 52531;
/// `ForwardMatrix3` (52532, 0xCD34) — camera-native → XYZ(D50) matrix for illuminant 3 (DNG 1.6).
pub const FORWARD_MATRIX3: u16 = 52532;
/// `IlluminantData1` (52533, 0xCD35) — spectral data for illuminant 1 when it is `Other` (DNG 1.6).
pub const ILLUMINANT_DATA1: u16 = 52533;
/// `IlluminantData2` (52534, 0xCD36) — spectral data for illuminant 2 when it is `Other` (DNG 1.6).
pub const ILLUMINANT_DATA2: u16 = 52534;
/// `IlluminantData3` (52535, 0xCD37) — spectral data for illuminant 3 when it is `Other` (DNG 1.6).
pub const ILLUMINANT_DATA3: u16 = 52535;
/// `MaskSubArea` (52536, 0xCD38) — the active sub-area a mask applies to (DNG 1.6).
pub const MASK_SUB_AREA: u16 = 52536;
/// `ProfileHueSatMapData3` (52537, 0xCD39) — hue/sat/value mapping data for illuminant 3 (DNG 1.6).
pub const PROFILE_HUE_SAT_MAP_DATA3: u16 = 52537;
/// `ReductionMatrix3` (52538, 0xCD3A) — dimension-reduction matrix for illuminant 3 (DNG 1.6).
pub const REDUCTION_MATRIX3: u16 = 52538;
/// `RGBTables` (52543, 0xCD3F) — 3D RGB look-up tables for the profile (DNG 1.6).
pub const RGB_TABLES: u16 = 52543;
/// `ProfileGainTableMap2` (52544, 0xCD40) — the revised profile gain-table map (DNG 1.7).
pub const PROFILE_GAIN_TABLE_MAP2: u16 = 52544;
/// The C2PA manifest store (52545, 0xCD41) — C2PA 2.4 §A.3.6, not a DNG tag.
///
/// A DNG may carry a content-credentials manifest store, and this crate writes and reads one
/// ([`DngMetadata::c2pa`](crate::DngMetadata::c2pa)), so a file carrying it is *recognised*
/// rather than flagged as private by [`is_known_tag`]. The number itself is
/// [`gamut_ifd::c2pa::C2PA_MANIFEST_STORE`] — the clause is stated once, in the crate shared by
/// every TIFF-based codec — and is aliased here only so [`KNOWN_TAGS`] can name it.
pub const C2PA_MANIFEST_STORE: u16 = gamut_ifd::c2pa::C2PA_MANIFEST_STORE;
/// `ColumnInterleaveFactor` (52547, 0xCD43) — column interleaving of the stored image data
/// (DNG 1.7.1).
pub const COLUMN_INTERLEAVE_FACTOR: u16 = 52547;
/// `ImageSequenceInfo` (52548, 0xCD44) — membership of a burst/bracket image sequence (DNG 1.7).
pub const IMAGE_SEQUENCE_INFO: u16 = 52548;
/// `ImageStats` (52550, 0xCD46) — pre-computed image statistics (DNG 1.7).
pub const IMAGE_STATS: u16 = 52550;
/// `ProfileDynamicRange` (52551, 0xCD47) — whether the profile targets SDR or HDR (DNG 1.7).
pub const PROFILE_DYNAMIC_RANGE: u16 = 52551;
/// `ProfileGroupName` (52552, 0xCD48) — the group name shared by related profiles (DNG 1.7).
pub const PROFILE_GROUP_NAME: u16 = 52552;
/// `JXLDistance` (52553, 0xCD49) — the Butteraugli distance used for JPEG XL encoding (DNG 1.7).
pub const JXL_DISTANCE: u16 = 52553;
/// `JXLEffort` (52554, 0xCD4A) — the JPEG XL encoder effort level (DNG 1.7).
pub const JXL_EFFORT: u16 = 52554;
/// `JXLDecodeSpeed` (52555, 0xCD4B) — the JPEG XL decode-speed tier (DNG 1.7).
pub const JXL_DECODE_SPEED: u16 = 52555;

/// `YCbCrCoefficients` (529, 0x0211) — RGB↔YCbCr transform coefficients of a YCbCr preview
/// (TIFF 6.0 §21; Adobe's own DNG samples carry YCbCr previews).
pub const YCBCR_COEFFICIENTS: u16 = 529;

/// `YCbCrSubSampling` (530, 0x0212) — chroma subsampling of a YCbCr preview (TIFF 6.0 §21).
pub const YCBCR_SUB_SAMPLING: u16 = 530;

/// `YCbCrPositioning` (531, 0x0213) — chroma sample positioning of a YCbCr preview
/// (TIFF 6.0 §21).
pub const YCBCR_POSITIONING: u16 = 531;

/// `ReferenceBlackWhite` (532, 0x0214) — headroom/footroom reference values of a YCbCr preview
/// (TIFF 6.0 §21).
pub const REFERENCE_BLACK_WHITE: u16 = 532;

/// Every tag this crate names — the source of truth for [`is_known_tag`].
///
/// Keep this in sync when adding a `pub const` above; [`tests::every_named_tag_is_known`] iterates
/// it but cannot, by itself, prove a newly-added constant was included (Rust has no const
/// reflection), so adding a tag here is part of adding the constant.
const KNOWN_TAGS: &[u16] = &[
    // Baseline TIFF 6.0 / TIFF-EP.
    NEW_SUBFILE_TYPE,
    IMAGE_WIDTH,
    IMAGE_LENGTH,
    BITS_PER_SAMPLE,
    COMPRESSION,
    PHOTOMETRIC_INTERPRETATION,
    IMAGE_DESCRIPTION,
    MAKE,
    MODEL,
    STRIP_OFFSETS,
    ORIENTATION,
    SAMPLES_PER_PIXEL,
    ROWS_PER_STRIP,
    STRIP_BYTE_COUNTS,
    X_RESOLUTION,
    Y_RESOLUTION,
    PLANAR_CONFIGURATION,
    PREDICTOR,
    RESOLUTION_UNIT,
    SOFTWARE,
    DATE_TIME,
    ARTIST,
    TILE_WIDTH,
    TILE_LENGTH,
    TILE_OFFSETS,
    TILE_BYTE_COUNTS,
    SUB_IFDS,
    EXTRA_SAMPLES,
    SAMPLE_FORMAT,
    YCBCR_COEFFICIENTS,
    YCBCR_SUB_SAMPLING,
    YCBCR_POSITIONING,
    REFERENCE_BLACK_WHITE,
    JPEG_TABLES,
    XMP,
    CFA_REPEAT_PATTERN_DIM,
    CFA_PATTERN,
    COPYRIGHT,
    IPTC_NAA,
    EXIF_IFD,
    ICC_PROFILE,
    GPS_INFO,
    // EXIF sub-IFD tags.
    EXPOSURE_TIME,
    F_NUMBER,
    ISO_SPEED_RATINGS,
    EXIF_VERSION,
    DATE_TIME_ORIGINAL,
    FOCAL_LENGTH,
    // DNG-private tags.
    DNG_VERSION,
    DNG_BACKWARD_VERSION,
    UNIQUE_CAMERA_MODEL,
    LOCALIZED_CAMERA_MODEL,
    CFA_PLANE_COLOR,
    CFA_LAYOUT,
    LINEARIZATION_TABLE,
    BLACK_LEVEL_REPEAT_DIM,
    BLACK_LEVEL,
    BLACK_LEVEL_DELTA_H,
    BLACK_LEVEL_DELTA_V,
    WHITE_LEVEL,
    DEFAULT_SCALE,
    DEFAULT_CROP_ORIGIN,
    DEFAULT_CROP_SIZE,
    COLOR_MATRIX1,
    COLOR_MATRIX2,
    CAMERA_CALIBRATION1,
    CAMERA_CALIBRATION2,
    REDUCTION_MATRIX1,
    REDUCTION_MATRIX2,
    ANALOG_BALANCE,
    AS_SHOT_NEUTRAL,
    AS_SHOT_WHITE_XY,
    BASELINE_EXPOSURE,
    BASELINE_NOISE,
    BASELINE_SHARPNESS,
    BAYER_GREEN_SPLIT,
    LINEAR_RESPONSE_LIMIT,
    CAMERA_SERIAL_NUMBER,
    LENS_INFO,
    CHROMA_BLUR_RADIUS,
    ANTI_ALIAS_STRENGTH,
    SHADOW_SCALE,
    DNG_PRIVATE_DATA,
    MAKER_NOTE_SAFETY,
    CALIBRATION_ILLUMINANT1,
    CALIBRATION_ILLUMINANT2,
    BEST_QUALITY_SCALE,
    RAW_DATA_UNIQUE_ID,
    ORIGINAL_RAW_FILE_NAME,
    ORIGINAL_RAW_FILE_DATA,
    ACTIVE_AREA,
    MASKED_AREAS,
    AS_SHOT_ICC_PROFILE,
    AS_SHOT_PRE_PROFILE_MATRIX,
    CURRENT_ICC_PROFILE,
    CURRENT_PRE_PROFILE_MATRIX,
    COLORIMETRIC_REFERENCE,
    CAMERA_CALIBRATION_SIGNATURE,
    PROFILE_CALIBRATION_SIGNATURE,
    EXTRA_CAMERA_PROFILES,
    AS_SHOT_PROFILE_NAME,
    NOISE_REDUCTION_APPLIED,
    PROFILE_NAME,
    PROFILE_HUE_SAT_MAP_DIMS,
    PROFILE_HUE_SAT_MAP_DATA1,
    PROFILE_HUE_SAT_MAP_DATA2,
    PROFILE_TONE_CURVE,
    PROFILE_EMBED_POLICY,
    PROFILE_COPYRIGHT,
    FORWARD_MATRIX1,
    FORWARD_MATRIX2,
    PREVIEW_APPLICATION_NAME,
    PREVIEW_APPLICATION_VERSION,
    PREVIEW_SETTINGS_NAME,
    PREVIEW_SETTINGS_DIGEST,
    PREVIEW_COLOR_SPACE,
    PREVIEW_DATE_TIME,
    RAW_IMAGE_DIGEST,
    ORIGINAL_RAW_FILE_DIGEST,
    SUB_TILE_BLOCK_SIZE,
    ROW_INTERLEAVE_FACTOR,
    PROFILE_LOOK_TABLE_DIMS,
    PROFILE_LOOK_TABLE_DATA,
    OPCODE_LIST1,
    OPCODE_LIST2,
    OPCODE_LIST3,
    NOISE_PROFILE,
    ORIGINAL_DEFAULT_FINAL_SIZE,
    ORIGINAL_BEST_QUALITY_FINAL_SIZE,
    ORIGINAL_DEFAULT_CROP_SIZE,
    PROFILE_HUE_SAT_MAP_ENCODING,
    PROFILE_LOOK_TABLE_ENCODING,
    BASELINE_EXPOSURE_OFFSET,
    DEFAULT_BLACK_RENDER,
    NEW_RAW_IMAGE_DIGEST,
    RAW_TO_PREVIEW_GAIN,
    DEFAULT_USER_CROP,
    // Depth-map tags.
    DEPTH_FORMAT,
    DEPTH_NEAR,
    DEPTH_FAR,
    DEPTH_UNITS,
    DEPTH_MEASURE_TYPE,
    ENHANCE_PARAMS,
    // DNG 1.6 / 1.7 additions.
    PROFILE_GAIN_TABLE_MAP,
    SEMANTIC_NAME,
    SEMANTIC_INSTANCE_ID,
    CALIBRATION_ILLUMINANT3,
    CAMERA_CALIBRATION3,
    COLOR_MATRIX3,
    FORWARD_MATRIX3,
    ILLUMINANT_DATA1,
    ILLUMINANT_DATA2,
    ILLUMINANT_DATA3,
    MASK_SUB_AREA,
    PROFILE_HUE_SAT_MAP_DATA3,
    REDUCTION_MATRIX3,
    RGB_TABLES,
    PROFILE_GAIN_TABLE_MAP2,
    C2PA_MANIFEST_STORE,
    COLUMN_INTERLEAVE_FACTOR,
    IMAGE_SEQUENCE_INFO,
    IMAGE_STATS,
    PROFILE_DYNAMIC_RANGE,
    PROFILE_GROUP_NAME,
    JXL_DISTANCE,
    JXL_EFFORT,
    JXL_DECODE_SPEED,
];

/// The 3×3 colour-calibration matrix tags whose value must be exactly nine `(S)RATIONAL`s — used by
/// a deconstruct to flag a malformed matrix.
pub const MATRIX_3X3_TAGS: &[u16] = &[
    COLOR_MATRIX1,
    COLOR_MATRIX2,
    COLOR_MATRIX3,
    CAMERA_CALIBRATION1,
    CAMERA_CALIBRATION2,
    CAMERA_CALIBRATION3,
    FORWARD_MATRIX1,
    FORWARD_MATRIX2,
    FORWARD_MATRIX3,
];

/// Whether `tag` is one this crate recognises as part of DNG 1.7.1 — the baseline TIFF/EP tags, the
/// EXIF tags it reads, and the DNG-private / depth / 1.6-1.7 tags — as opposed to a private or
/// unknown tag a strict deconstruct should flag.
///
/// A handful of common baseline TIFF tags a DNG may carry but the codec does not name
/// (`HostComputer`, `Min`/`MaxSampleValue`) are also accepted so a valid file is not flagged for
/// them.
///
/// # What "known" means here
///
/// **A tag this crate recognises** — a question about this crate's vocabulary, answered from the
/// tag number alone. It is deliberately *not* "a tag this decode consumed": that would make the
/// answer depend on the file's contents, so the same tag would be known in one file and unknown
/// in another, and `deconstruct`'s `UnknownTag` would stop meaning "a private or unrecognised
/// tag" and start meaning "a tag some code path declined". Several listed tags are recognised
/// but not modelled, and reach the caller verbatim as [`RawTag`](crate::RawTag)s; the C2PA
/// manifest store (52545) is recognised here even though its clause is C2PA's rather than DNG's,
/// because this crate writes and reads one.
#[must_use]
pub fn is_known_tag(tag: u16) -> bool {
    KNOWN_TAGS.contains(&tag)
        || matches!(
            tag,
            280 // MinSampleValue
            | 281 // MaxSampleValue
            | 316 // HostComputer
            | 320 // ColorMap (palette previews)
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_tag_is_known() {
        for &tag in KNOWN_TAGS {
            assert!(is_known_tag(tag), "tag {tag} should be known");
        }
        // A couple of the matrix tags double as a spot-check that the list and accessors agree.
        assert!(is_known_tag(COLOR_MATRIX1));
        assert!(is_known_tag(JXL_DECODE_SPEED));
        // The C2PA store is a tag this crate writes, so a file carrying one is not "private":
        // `is_fully_accounted` must stay true for a DNG this encoder produced.
        assert!(is_known_tag(gamut_ifd::c2pa::C2PA_MANIFEST_STORE));
    }

    #[test]
    fn private_and_unassigned_tags_are_unknown() {
        assert!(!is_known_tag(0x9999));
        assert!(!is_known_tag(50341)); // PrintImageMatching (proprietary)
        assert!(!is_known_tag(1)); // not a DNG tag
    }
}
