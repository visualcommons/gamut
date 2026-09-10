// A minimal `extern "C"` shim over the Adobe DNG SDK, mirroring the parse/read flow of the SDK's
// own `dng_validate` tool (source/dng_validate.cpp): open the file, parse the IFDs, build a
// negative, and read its stage-1 (raw) image. If any of that throws, the file is not a valid DNG
// the reference implementation accepts.

#include "dng_auto_ptr.h"
#include "dng_camera_profile.h"
#include "dng_color_spec.h"
#include "dng_errors.h"
#include "dng_exceptions.h"
#include "dng_file_stream.h"
#include "dng_fingerprint.h"
#include "dng_host.h"
#include "dng_image.h"
#include "dng_info.h"
#include "dng_lossless_jpeg.h"
#include "dng_negative.h"
#include "dng_pixel_buffer.h"
#include "dng_rect.h"
#include "dng_simd_type.h"
#include "dng_stream.h"
#include "dng_tag_types.h"

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <vector>

namespace {

// Parses `path` into a negative and reads its stage-1 (raw) image. Shared by the entry points.
dng_error_code read_negative(const char *path, dng_host &host, dng_info &info,
                             AutoPtr<dng_negative> &negative) {
  dng_file_stream stream(path);
  info.Parse(host, stream);
  info.PostParse(host);
  if (!info.IsValidDNG()) {
    return dng_error_bad_format;
  }
  negative.Reset(host.Make_dng_negative());
  negative->Parse(host, stream, info);
  negative->PostParse(host, stream, info);
  negative->ReadStage1Image(host, stream, info);
  return dng_error_none;
}

// Copies a 16-bit-typed `dng_image` into a freshly `malloc`d interleaved `uint16` buffer,
// filling the out-parameters. Returns `dng_error_none` on success.
dng_error_code copy_short_image(const dng_image *image, uint32_t *out_w, uint32_t *out_h,
                                uint32_t *out_planes, uint16_t **out_data, size_t *out_len) {
  if (image == nullptr) {
    return dng_error_unknown;
  }
  if (image->PixelType() != ttShort) {
    return dng_error_unsupported_dng;
  }
  dng_rect bounds = image->Bounds();
  uint32 w = static_cast<uint32>(bounds.r - bounds.l);
  uint32 h = static_cast<uint32>(bounds.b - bounds.t);
  uint32 planes = image->Planes();
  size_t count = static_cast<size_t>(w) * static_cast<size_t>(h) * static_cast<size_t>(planes);
  uint16_t *buffer = static_cast<uint16_t *>(malloc(count * sizeof(uint16_t)));
  if (buffer == nullptr) {
    return dng_error_memory;
  }
  dng_pixel_buffer pb;
  pb.fArea = bounds;
  pb.fPlane = 0;
  pb.fPlanes = planes;
  pb.fRowStep = static_cast<int32>(static_cast<size_t>(w) * planes);
  pb.fColStep = static_cast<int32>(planes);
  pb.fPlaneStep = 1;
  pb.fPixelType = ttShort;
  pb.fPixelSize = static_cast<uint32>(sizeof(uint16_t));
  pb.fData = buffer;
  image->Get(pb);
  *out_w = w;
  *out_h = h;
  *out_planes = planes;
  *out_data = buffer;
  *out_len = count;
  return dng_error_none;
}

} // namespace

// The code gdng_validate returns when the SDK marks the negative damaged (a stored
// RawImageDigest/NewRawImageDigest that does not match the image data). The SDK's non-validate
// build records this via SetIsDamaged rather than throwing, so it must be surfaced explicitly.
#define GDNG_ERROR_DAMAGED 1

// Validates the DNG at `path`, returning `dng_error_none` (0) if the Adobe SDK parses and reads it
// without error and any stored raw digest matches the image data; `GDNG_ERROR_DAMAGED` on a digest
// mismatch; or the SDK error code otherwise.
extern "C" int gdng_validate(const char *path) {
  try {
    dng_host host;
    dng_info info;
    AutoPtr<dng_negative> negative;
    dng_error_code rc = read_negative(path, host, info, negative);
    if (rc != dng_error_none) {
      return rc;
    }
    negative->ValidateRawImageDigest(host);
    if (negative->IsDamaged()) {
      return GDNG_ERROR_DAMAGED;
    }
  } catch (const dng_exception &except) {
    return except.ErrorCode();
  } catch (...) {
    return dng_error_unknown;
  }

  return dng_error_none;
}

// Reads the DNG at `path` and returns its stage-1 (raw) image samples — the sensor values as
// stored, before linearisation/black-subtraction — as a freshly `malloc`d interleaved `uint16`
// buffer (`width * height * planes`), which the caller must release with `gdng_free`. Returns
// `dng_error_none` on success, or the SDK error code (the raw must be a 16-bit-typed image).
extern "C" int gdng_read_raw(const char *path, uint32_t *out_w, uint32_t *out_h,
                             uint32_t *out_planes, uint16_t **out_data, size_t *out_len) {
  *out_data = nullptr;
  *out_w = 0;
  *out_h = 0;
  *out_planes = 0;
  *out_len = 0;
  try {
    dng_host host;
    dng_info info;
    AutoPtr<dng_negative> negative;
    dng_error_code rc = read_negative(path, host, info, negative);
    if (rc != dng_error_none) {
      return rc;
    }
    return copy_short_image(negative->Stage1Image(), out_w, out_h, out_planes, out_data, out_len);
  } catch (const dng_exception &except) {
    return except.ErrorCode();
  } catch (...) {
    return dng_error_unknown;
  }
}

// Decodes the DNG held in `data`/`len` and reports the extent of the stage-1 (raw) image it
// produced, without exporting the samples. This is the *timed* decode entry point (issue #163):
// it exists so a throughput benchmark can compare the reference implementation against gamut's
// `DngDecoder` on the same terms, and it differs from `gdng_read_raw` in exactly two ways, both
// of which remove work gamut's decoder does not do either:
//
//   * it reads from a memory stream rather than a `dng_file_stream`, so no temporary file is
//     written and no filesystem is touched inside the measured region, and
//   * it stops once `ReadStage1Image` has materialised the image, skipping the
//     `copy_short_image` export pass — an extra full-image `malloc` + `memcpy` that only the FFI
//     boundary needs.
//
// Everything else is the same parse → build-negative → read-stage-1 flow as `gdng_read_raw`.
// Returns `dng_error_none` on success, or the SDK error code.
extern "C" int gdng_decode_dng_in_memory(const uint8_t *data, size_t len, uint32_t *out_w,
                                         uint32_t *out_h, uint32_t *out_planes, size_t *out_len) {
  *out_w = 0;
  *out_h = 0;
  *out_planes = 0;
  *out_len = 0;
  if (len > 0xFFFFFFFFu) {
    return dng_error_bad_format;
  }
  try {
    dng_host host;
    dng_info info;
    dng_stream stream(data, static_cast<uint32>(len));
    info.Parse(host, stream);
    info.PostParse(host);
    if (!info.IsValidDNG()) {
      return dng_error_bad_format;
    }
    AutoPtr<dng_negative> negative(host.Make_dng_negative());
    negative->Parse(host, stream, info);
    negative->PostParse(host, stream, info);
    negative->ReadStage1Image(host, stream, info);
    const dng_image *image = negative->Stage1Image();
    if (image == nullptr) {
      return dng_error_unknown;
    }
    dng_rect bounds = image->Bounds();
    uint32 w = static_cast<uint32>(bounds.r - bounds.l);
    uint32 h = static_cast<uint32>(bounds.b - bounds.t);
    uint32 planes = image->Planes();
    *out_w = w;
    *out_h = h;
    *out_planes = planes;
    *out_len = static_cast<size_t>(w) * static_cast<size_t>(h) * static_cast<size_t>(planes);
  } catch (const dng_exception &except) {
    return except.ErrorCode();
  } catch (...) {
    return dng_error_unknown;
  }
  return dng_error_none;
}

// Reads the DNG at `path` and returns its stage-2 (linearized) image — the SDK's application of
// the spec's Chapter-5 "Mapping Raw Values to Linear Reference Values": linearization table,
// black subtraction (pattern + deltas), rescale, clip. The buffer is active-area-sized,
// interleaved `uint16` where 0..65535 encodes linear 0.0..1.0 (the default host preserves no
// black levels, so 0 is black). Caller frees with `gdng_free`. Returns `dng_error_none` on
// success or the SDK error code.
extern "C" int gdng_read_linear(const char *path, uint32_t *out_w, uint32_t *out_h,
                                uint32_t *out_planes, uint16_t **out_data, size_t *out_len) {
  *out_data = nullptr;
  *out_w = 0;
  *out_h = 0;
  *out_planes = 0;
  *out_len = 0;
  try {
    dng_host host;
    dng_info info;
    AutoPtr<dng_negative> negative;
    dng_error_code rc = read_negative(path, host, info, negative);
    if (rc != dng_error_none) {
      return rc;
    }
    negative->BuildStage2Image(host);
    return copy_short_image(negative->Stage2Image(), out_w, out_h, out_planes, out_data, out_len);
  } catch (const dng_exception &except) {
    return except.ErrorCode();
  } catch (...) {
    return dng_error_unknown;
  }
}

// Computes the SDK's `NewRawImageDigest` (the MD5-over-raw-image algorithm of
// `dng_negative::FindNewRawImageDigest`) for the DNG at `path`, writing the 16 digest bytes to
// `out_digest`. This is the reference for gamut-dng's own digest writer. Returns `dng_error_none`
// on success or the SDK error code.
extern "C" int gdng_new_raw_image_digest(const char *path, uint8_t *out_digest) {
  try {
    dng_host host;
    dng_info info;
    AutoPtr<dng_negative> negative;
    dng_error_code rc = read_negative(path, host, info, negative);
    if (rc != dng_error_none) {
      return rc;
    }
    // A digest parsed from the file itself must not short-circuit the computation —
    // FindNewRawImageDigest is a no-op when the negative already carries one, which would turn
    // this differential oracle into a comparison of the caller's value with itself.
    negative->ClearRawImageDigest();
    negative->FindNewRawImageDigest(host);
    const dng_fingerprint &digest = negative->NewRawImageDigest();
    memcpy(out_digest, digest.Data(), 16);
  } catch (const dng_exception &except) {
    return except.ErrorCode();
  } catch (...) {
    return dng_error_unknown;
  }
  return dng_error_none;
}

namespace {

// Collects the rows DecodeLosslessJPEG spools (native-endian interleaved uint16).
class buffer_spooler : public dng_spooler {
public:
  std::vector<uint8_t> bytes;
  void Spool(const void *data, uint32 count) override {
    const uint8_t *p = static_cast<const uint8_t *>(data);
    bytes.insert(bytes.end(), p, p + count);
  }
};

} // namespace

// Decodes a bare lossless-JPEG (SOF3) stream with the SDK's own codec
// (`DecodeLosslessJPEG<Scalar>`), the reference for gamut-dng's process-14 decoder. The caller
// supplies the expected sample count (width * height * components) as the decode-size bound; the
// interleaved `uint16` samples land in a freshly `malloc`d buffer released with `gdng_free`.
// Returns `dng_error_none` on success or the SDK error code.
extern "C" int gdng_decode_lossless_jpeg(const uint8_t *data, size_t len, size_t expected_samples,
                                         uint16_t **out_data, size_t *out_len) {
  *out_data = nullptr;
  *out_len = 0;
  try {
    dng_stream stream(data, static_cast<uint32>(len));
    buffer_spooler spooler;
    uint32 byte_count = static_cast<uint32>(expected_samples * sizeof(uint16_t));
    DecodeLosslessJPEG<Scalar>(stream, spooler, byte_count, byte_count, false,
                               static_cast<uint64>(len));
    if (spooler.bytes.size() != byte_count) {
      return dng_error_bad_format;
    }
    uint16_t *buffer = static_cast<uint16_t *>(malloc(byte_count));
    if (buffer == nullptr) {
      return dng_error_memory;
    }
    memcpy(buffer, spooler.bytes.data(), byte_count);
    *out_data = buffer;
    *out_len = expected_samples;
    return dng_error_none;
  } catch (const dng_exception &except) {
    return except.ErrorCode();
  } catch (...) {
    return dng_error_unknown;
  }
}

// Releases a buffer returned by `gdng_read_raw` / `gdng_read_linear` /
// `gdng_decode_lossless_jpeg`.
// Returns the camera-neutral coordinates the reference implementation derives for the DNG at
// `path`, as `*out_channels` doubles written to `out_neutral` (room for `kMaxColorPlanes`).
//
// For a file whose white balance is stored as `AsShotWhiteXY` (50729) rather than `AsShotNeutral`
// (50728), that derivation *is* the DNG spec's "Translating White Balance xy Coordinates to Camera
// Neutral Coordinates" (dng_color_spec::SetWhiteXY), so this is the oracle for that conversion.
// The SDK reads 50729 only when 50728 is absent, so a file carrying neither — or carrying the
// neutral — is reported as `dng_error_bad_format` rather than silently answering for the wrong tag.
extern "C" int gdng_camera_neutral_from_white_xy(const char *path, uint32_t *out_channels,
                                                 double *out_neutral) {
  try {
    dng_host host;
    dng_info info;
    AutoPtr<dng_negative> negative;
    dng_error_code rc = read_negative(path, host, info, negative);
    if (rc != dng_error_none) {
      return rc;
    }
    if (!negative->HasCameraWhiteXY()) {
      return dng_error_bad_format;
    }
    AutoPtr<dng_color_spec> spec(negative->MakeColorSpec(dng_camera_profile_id()));
    spec->SetWhiteXY(negative->CameraWhiteXY());
    const dng_vector &white = spec->CameraWhite();
    uint32 channels = white.Count();
    if (channels == 0 || channels > kMaxColorPlanes) {
      return dng_error_unsupported_dng;
    }
    for (uint32 index = 0; index < channels; index++) {
      out_neutral[index] = white[index];
    }
    *out_channels = channels;
  } catch (const dng_exception &except) {
    return except.ErrorCode();
  } catch (...) {
    return dng_error_unknown;
  }

  return dng_error_none;
}

extern "C" void gdng_free(uint16_t *data) { free(data); }
