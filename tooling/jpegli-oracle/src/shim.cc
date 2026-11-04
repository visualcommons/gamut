// A small `extern "C"` bridge over jpegli's renamed libjpeg API (libjxl v0.11.2, `lib/jpegli`).
//
// jpegli signals fatal errors by calling `error_exit`, which must not return; as with libjpeg, the
// idiom is to `longjmp` back to a `setjmp` established before the operation. That control flow
// cannot be expressed in Rust, so each operation lives here — Rust sees return codes and, on
// success, `malloc`'d buffers released through `oracle_jpegli_free`. Every object with a
// non-trivial destructor is constructed *before* the `setjmp`, so a `longjmp` skips no destructor.
//
// The XYB encode reproduces what libjxl's `extras/enc/jpegli.cc` (`EncodeJpeg`, the code behind
// `cjpegli --xyb`) does around jpegli, which itself never converts colour in XYB mode: sRGB samples
// → linear sRGB → opsin XYB (`LinearRGBRowToXYB`) → scaled XYB (`ScaleXYBRow`), fed to jpegli as
// floats with `jpegli_set_xyb_mode`, and the profile libjxl's `ColorEncoding::CreateICC` makes for
// the XYB encoding embedded via `jpegli_write_icc_profile`. The two transforms are transcribed as
// scalar code from `lib/jxl/enc_xyb.cc` over libjxl's own constants (`lib/jxl/cms/opsin_params.h`);
// the profile is produced by libjxl's header-only `MaybeCreateProfile` itself.

#include <setjmp.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <cmath>
#include <vector>

#include <jxl/color_encoding.h>

#include "lib/jpegli/decode.h"
#include "lib/jpegli/encode.h"
#include "lib/jxl/cms/jxl_cms_internal.h"
#include "lib/jxl/cms/opsin_params.h"

namespace {

// jpeg_error_mgr extended with the jump target `error_exit` longjmps to.
struct OracleErrorMgr {
  jpeg_error_mgr pub;
  jmp_buf env;
};

void OracleErrorExit(j_common_ptr cinfo) {
  OracleErrorMgr* err = reinterpret_cast<OracleErrorMgr*>(cinfo->err);
  longjmp(err->env, 1);
}

// Swallows jpegli's warnings; the Rust caller only cares about the return code.
void OracleEmitMessage(j_common_ptr cinfo, int msg_level) {
  (void)cinfo;
  (void)msg_level;
}

// The IEC 61966-2-1 sRGB EOTF on a [0, 1] encoded value.
float SrgbToLinear(float v) {
  return v <= 0.04045f ? v / 12.92f : std::pow((v + 0.055f) / 1.055f, 2.4f);
}

// libjxl's profile for the scaled-XYB encoding (`ColorEncoding::SetColorSpace(kXYB)` with the
// perceptual intent, as `EncodeJpeg` sets it): D65 white for the `chad` tag, sRGB primaries and
// transfer as the defaults a `ColorEncoding` carries.
bool XybProfile(std::vector<uint8_t>* icc) {
  JxlColorEncoding c;
  memset(&c, 0, sizeof(c));
  c.color_space = JXL_COLOR_SPACE_XYB;
  c.white_point = JXL_WHITE_POINT_D65;
  c.white_point_xy[0] = 0.3127;
  c.white_point_xy[1] = 0.3290;
  c.primaries = JXL_PRIMARIES_SRGB;
  c.primaries_red_xy[0] = 0.639998686;
  c.primaries_red_xy[1] = 0.330010138;
  c.primaries_green_xy[0] = 0.300003784;
  c.primaries_green_xy[1] = 0.600003357;
  c.primaries_blue_xy[0] = 0.150002046;
  c.primaries_blue_xy[1] = 0.059997204;
  c.transfer_function = JXL_TRANSFER_FUNCTION_SRGB;
  c.rendering_intent = JXL_RENDERING_INTENT_PERCEPTUAL;
  return static_cast<bool>(jxl::MaybeCreateProfile(c, icc));
}

// `LinearRGBRowToXYB` then `ScaleXYBRow` (lib/jxl/enc_xyb.cc) for one pixel, in place.
void LinearToScaledXyb(float rgb[3]) {
  const auto& m = jxl::cms::kOpsinAbsorbanceMatrix;
  const auto& bias = jxl::cms::kOpsinAbsorbanceBias;
  float mixed[3];
  for (int j = 0; j < 3; ++j) {
    float v = m[j][0] * rgb[0] + m[j][1] * rgb[1] + m[j][2] * rgb[2] + bias[j];
    v = v < 0.0f ? 0.0f : v;  // ZeroIfNegative
    mixed[j] = std::cbrt(v) - std::cbrt(bias[j]);
  }
  const float x = 0.5f * (mixed[0] - mixed[1]);
  const float y = 0.5f * (mixed[0] + mixed[1]);
  const float b = mixed[2];
  rgb[2] = (b - y + jxl::cms::kScaledXYBOffset[2]) * jxl::cms::kScaledXYBScale[2];
  rgb[0] = (x + jxl::cms::kScaledXYBOffset[0]) * jxl::cms::kScaledXYBScale[0];
  rgb[1] = (y + jxl::cms::kScaledXYBOffset[1]) * jxl::cms::kScaledXYBScale[1];
}

}  // namespace

extern "C" {

// Releases a buffer returned by any oracle_jpegli_* function.
void oracle_jpegli_free(uint8_t* p) { free(p); }

// libjxl's XYB ICC profile (see XybProfile). Returns 0 on success with a malloc'd `*out`.
int oracle_jpegli_xyb_icc(uint8_t** out, size_t* out_len) {
  std::vector<uint8_t> icc;
  if (!XybProfile(&icc)) return 1;
  uint8_t* buf = static_cast<uint8_t*>(malloc(icc.size()));
  if (buf == nullptr) return 2;
  memcpy(buf, icc.data(), icc.size());
  *out = buf;
  *out_len = icc.size();
  return 0;
}

// Encodes `rgb` (interleaved sRGB, `width * height * 3` bytes) at jpegli `quality`, mapped to a
// butteraugli distance by `jpegli_quality_to_distance` as `cjpegli -q` does.
//
// With `xyb != 0` the stream is jpegli's XYB mode (see the file header). jpegli's default XYB
// sampling is X 2x2, Y 2x2, B 1x1 (`jpegli_set_colorspace`, encode.cc), i.e. only B is
// subsampled, 2:1 in both directions; `xyb_full_res != 0` overrides every component to 1x1. Without
// `xyb` the stream is YCbCr at jpegli's own 4:4:4 default (`EncodeJpeg`'s non-XYB default). Other
// settings are `cjpegli`'s defaults: adaptive quantization on, optimized Huffman coding, and
// progressive level 2.
//
// Returns 0 on success (malloc'd `*out`), 1 on a jpegli error, 2 on a profile/allocation failure.
int oracle_jpegli_encode(const uint8_t* rgb, unsigned width, unsigned height, int quality, int xyb,
                         int xyb_full_res, uint8_t** out, size_t* out_len) {
  std::vector<uint8_t> icc;
  std::vector<float> row(static_cast<size_t>(width) * 3);
  if (xyb && !XybProfile(&icc)) return 2;

  jpeg_compress_struct cinfo;
  OracleErrorMgr jerr;
  unsigned char* volatile mem = nullptr;
  unsigned long mem_size = 0;  // NOLINT: jpegli_mem_dest's out-param type.

  cinfo.err = jpegli_std_error(&jerr.pub);
  jerr.pub.error_exit = OracleErrorExit;
  jerr.pub.emit_message = OracleEmitMessage;
  if (setjmp(jerr.env)) {
    jpegli_destroy_compress(&cinfo);
    free(mem);
    return 1;
  }
  jpegli_create_compress(&cinfo);
  jpegli_mem_dest(&cinfo, const_cast<unsigned char**>(&mem), &mem_size);
  cinfo.image_width = width;
  cinfo.image_height = height;
  cinfo.input_components = 3;
  cinfo.in_color_space = JCS_RGB;
  if (xyb) jpegli_set_xyb_mode(&cinfo);
  jpegli_set_defaults(&cinfo);
  if (!xyb || xyb_full_res) {
    for (int c = 0; c < cinfo.num_components; ++c) {
      cinfo.comp_info[c].h_samp_factor = 1;
      cinfo.comp_info[c].v_samp_factor = 1;
    }
  }
  jpegli_enable_adaptive_quantization(&cinfo, TRUE);
  jpegli_set_distance(&cinfo, jpegli_quality_to_distance(quality), TRUE);
  jpegli_set_progressive_level(&cinfo, 2);
  cinfo.optimize_coding = TRUE;
  if (xyb) jpegli_set_input_format(&cinfo, JPEGLI_TYPE_FLOAT, JPEGLI_NATIVE_ENDIAN);
  jpegli_start_compress(&cinfo, TRUE);
  if (xyb) jpegli_write_icc_profile(&cinfo, icc.data(), static_cast<unsigned int>(icc.size()));
  for (unsigned y = 0; y < height; ++y) {
    const uint8_t* src = rgb + static_cast<size_t>(y) * width * 3;
    JSAMPROW rows[1];
    if (xyb) {
      for (unsigned x = 0; x < width; ++x) {
        float px[3];
        for (int c = 0; c < 3; ++c) px[c] = SrgbToLinear(src[x * 3 + c] / 255.0f);
        LinearToScaledXyb(px);
        memcpy(&row[x * 3], px, sizeof(px));
      }
      rows[0] = reinterpret_cast<JSAMPROW>(row.data());
    } else {
      rows[0] = const_cast<JSAMPROW>(src);
    }
    jpegli_write_scanlines(&cinfo, rows, 1);
  }
  jpegli_finish_compress(&cinfo);
  jpegli_destroy_compress(&cinfo);
  *out = mem;
  *out_len = mem_size;
  return 0;
}

// Decodes `data` with jpegli's defaults (its own colour-space decision, fancy upsampling, float
// IDCT): a three-component stream with Adobe `transform = 0` or component ids R,G,B — every XYB
// stream — comes out as its stored samples, untransformed. jpegli applies no ICC profile; the
// embedded one is returned beside the samples (`*icc = NULL` when absent).
//
// Returns 0 on success (malloc'd `*pixels` and, when present, `*icc`), 1 on a jpegli error, 2 on
// allocation failure.
int oracle_jpegli_decode(const uint8_t* data, size_t len, unsigned* width, unsigned* height,
                         unsigned* channels, uint8_t** pixels, size_t* pixels_len, uint8_t** icc,
                         size_t* icc_len) {
  jpeg_decompress_struct cinfo;
  OracleErrorMgr jerr;
  uint8_t* volatile buf = nullptr;
  JOCTET* volatile profile = nullptr;

  cinfo.err = jpegli_std_error(&jerr.pub);
  jerr.pub.error_exit = OracleErrorExit;
  jerr.pub.emit_message = OracleEmitMessage;
  if (setjmp(jerr.env)) {
    jpegli_destroy_decompress(&cinfo);
    free(buf);
    free(profile);
    return 1;
  }
  jpegli_create_decompress(&cinfo);
  jpegli_mem_src(&cinfo, data, static_cast<unsigned long>(len));
  jpegli_save_markers(&cinfo, JPEG_APP0 + 2, 0xFFFF);
  jpegli_read_header(&cinfo, TRUE);
  unsigned int profile_len = 0;
  JOCTET* profile_ptr = nullptr;
  if (jpegli_read_icc_profile(&cinfo, &profile_ptr, &profile_len)) profile = profile_ptr;
  jpegli_start_decompress(&cinfo);
  const unsigned w = cinfo.output_width;
  const unsigned h = cinfo.output_height;
  const unsigned ch = static_cast<unsigned>(cinfo.output_components);
  const size_t stride = static_cast<size_t>(w) * ch;
  const size_t total = stride * h;
  buf = static_cast<uint8_t*>(malloc(total ? total : 1));
  if (buf == nullptr) {
    jpegli_destroy_decompress(&cinfo);
    free(profile);
    return 2;
  }
  while (cinfo.output_scanline < cinfo.output_height) {
    JSAMPROW row = buf + static_cast<size_t>(cinfo.output_scanline) * stride;
    jpegli_read_scanlines(&cinfo, &row, 1);
  }
  jpegli_finish_decompress(&cinfo);
  jpegli_destroy_decompress(&cinfo);
  *width = w;
  *height = h;
  *channels = ch;
  *pixels = buf;
  *pixels_len = total;
  *icc = profile;
  *icc_len = profile ? profile_len : 0;
  return 0;
}

}  // extern "C"
