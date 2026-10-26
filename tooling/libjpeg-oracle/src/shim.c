/* A tiny `extern "C"` bridge over libjpeg-turbo's classic in-memory API.
 *
 * libjpeg signals fatal errors by calling `error_exit`, which must not return: the canonical
 * idiom is to `longjmp` back to a `setjmp` established before the operation. That control-flow
 * trick cannot be expressed in Rust, so the whole encode/decode lives here in C — Rust only sees
 * success/failure return codes and (on success) a malloc'd output buffer freed via `oracle_free`.
 *
 * Buffers handed back to Rust are plain `malloc` allocations (decode) or libjpeg's own
 * `jpeg_mem_dest` allocation (encode, also `malloc`-based); both are released with `free` in
 * `oracle_free`.
 */

#include <setjmp.h>
#include <stddef.h>
#include <stdio.h> /* jpeglib.h declares FILE-based helpers (jpeg_stdio_src/dest) */
#include <stdlib.h>

#include <jpeglib.h>

/* jconfig.h defines LIBJPEG_TURBO_VERSION as the bare token 3.2.0 (not a string), so stringize it. */
#define ORACLE_STR(x) #x
#define ORACLE_XSTR(x) ORACLE_STR(x)

/* jpeg_error_mgr extended with the jump target `error_exit` longjmps to. */
struct oracle_error_mgr {
  struct jpeg_error_mgr pub;
  jmp_buf setjmp_buffer;
};

/* Fatal-error hook: jump back to the setjmp in the enclosing operation instead of exiting. */
static void oracle_error_exit(j_common_ptr cinfo) {
  struct oracle_error_mgr *err = (struct oracle_error_mgr *)cinfo->err;
  longjmp(err->setjmp_buffer, 1);
}

/* Swallow libjpeg's stderr chatter (warnings / "Corrupt JPEG data" on the negative decode tests):
 * the Rust caller only cares about the return code. */
static void oracle_emit_message(j_common_ptr cinfo, int msg_level) {
  (void)cinfo;
  (void)msg_level;
}

/* The linked libjpeg-turbo version, e.g. "3.2.0". */
const char *oracle_jpeg_version(void) { return ORACLE_XSTR(LIBJPEG_TURBO_VERSION); }

/* Releases a buffer returned by oracle_jpeg_decode / oracle_jpeg_encode. */
void oracle_free(unsigned char *p) { free(p); }

/* Decodes `data` into interleaved 8-bit samples.
 *
 * With `force_rgb == 0` libjpeg's default out_color_space stands (grayscale -> 1 channel,
 * YCbCr -> RGB 3, CMYK/YCCK -> 4); with `force_rgb != 0` the output is forced to RGB.
 * The default IDCT is left at libjpeg's default. Fancy upsampling is too, unless `no_fancy != 0`
 * turns it off (`djpeg -nosmooth`): every subsampled component is then replicated.
 *
 * Returns 0 on success (and fills the out-params + a malloc'd `*out_pixels`), 1 on a libjpeg error,
 * 2 on allocation failure. */
int oracle_jpeg_decode(const unsigned char *data, size_t len, int force_rgb, int no_fancy,
                       unsigned int *out_width, unsigned int *out_height,
                       unsigned int *out_channels, unsigned char **out_pixels,
                       size_t *out_len) {
  struct jpeg_decompress_struct cinfo;
  struct oracle_error_mgr jerr;
  /* volatile: read again in the longjmp handler, so it must not be cached in a register. */
  unsigned char *volatile buf = NULL;

  cinfo.err = jpeg_std_error(&jerr.pub);
  jerr.pub.error_exit = oracle_error_exit;
  jerr.pub.emit_message = oracle_emit_message;
  if (setjmp(jerr.setjmp_buffer)) {
    jpeg_destroy_decompress(&cinfo);
    free(buf);
    return 1;
  }

  jpeg_create_decompress(&cinfo);
  jpeg_mem_src(&cinfo, data, (unsigned long)len);
  jpeg_read_header(&cinfo, TRUE);
  if (force_rgb) {
    cinfo.out_color_space = JCS_RGB;
  }
  if (no_fancy) {
    cinfo.do_fancy_upsampling = FALSE;
  }
  jpeg_start_decompress(&cinfo);

  unsigned int width = cinfo.output_width;
  unsigned int height = cinfo.output_height;
  unsigned int channels = (unsigned int)cinfo.output_components;
  size_t row_stride = (size_t)width * channels;
  size_t total = row_stride * height;

  buf = (unsigned char *)malloc(total ? total : 1);
  if (buf == NULL) {
    jpeg_destroy_decompress(&cinfo);
    return 2;
  }

  while (cinfo.output_scanline < cinfo.output_height) {
    unsigned char *rowptr = buf + (size_t)cinfo.output_scanline * row_stride;
    jpeg_read_scanlines(&cinfo, &rowptr, 1);
  }

  jpeg_finish_decompress(&cinfo);
  jpeg_destroy_decompress(&cinfo);

  *out_width = width;
  *out_height = height;
  *out_channels = channels;
  *out_pixels = buf;
  *out_len = total;
  return 0;
}

/* Reads the ICC profile of a JPEG via jpeg_read_icc_profile (which reassembles the APP2
 * ICC_PROFILE chunk sequence). Only the header is parsed; no pixels are decoded.
 *
 * Returns 0 on success. A stream without a profile succeeds with `*out = NULL, *out_len = 0`;
 * the returned buffer is malloc'd by libjpeg-turbo and released via `oracle_free`. */
int oracle_jpeg_read_icc(const unsigned char *data, size_t len, unsigned char **out,
                         size_t *out_len) {
  struct jpeg_decompress_struct cinfo;
  struct oracle_error_mgr jerr;
  JOCTET *volatile icc = NULL;

  cinfo.err = jpeg_std_error(&jerr.pub);
  jerr.pub.error_exit = oracle_error_exit;
  jerr.pub.emit_message = oracle_emit_message;
  if (setjmp(jerr.setjmp_buffer)) {
    jpeg_destroy_decompress(&cinfo);
    free(icc);
    return 1;
  }

  jpeg_create_decompress(&cinfo);
  jpeg_mem_src(&cinfo, data, (unsigned long)len);
  /* jpeg_read_icc_profile only sees APP2 markers saved before the header is read. */
  jpeg_save_markers(&cinfo, JPEG_APP0 + 2, 0xFFFF);
  jpeg_read_header(&cinfo, TRUE);

  unsigned int icc_len = 0;
  JOCTET *icc_local = NULL;
  if (jpeg_read_icc_profile(&cinfo, &icc_local, &icc_len)) {
    icc = icc_local;
    *out = icc_local;
    *out_len = (size_t)icc_len;
  } else {
    *out = NULL;
    *out_len = 0;
  }
  jpeg_destroy_decompress(&cinfo);
  return 0;
}

/* Captures the raw payload of the first APP1 marker segment (length bytes excluded), e.g. the
 * "Exif\0\0" + TIFF blob a writer embedded. Only the header is parsed.
 *
 * Returns 0 on success. A stream without an APP1 succeeds with `*out = NULL, *out_len = 0`; the
 * returned buffer is malloc'd and released via `oracle_free`. */
int oracle_jpeg_read_app1(const unsigned char *data, size_t len, unsigned char **out,
                          size_t *out_len) {
  struct jpeg_decompress_struct cinfo;
  struct oracle_error_mgr jerr;
  unsigned char *volatile buf = NULL;

  cinfo.err = jpeg_std_error(&jerr.pub);
  jerr.pub.error_exit = oracle_error_exit;
  jerr.pub.emit_message = oracle_emit_message;
  if (setjmp(jerr.setjmp_buffer)) {
    jpeg_destroy_decompress(&cinfo);
    free(buf);
    return 1;
  }

  jpeg_create_decompress(&cinfo);
  jpeg_mem_src(&cinfo, data, (unsigned long)len);
  jpeg_save_markers(&cinfo, JPEG_APP0 + 1, 0xFFFF);
  jpeg_read_header(&cinfo, TRUE);

  *out = NULL;
  *out_len = 0;
  for (jpeg_saved_marker_ptr m = cinfo.marker_list; m != NULL; m = m->next) {
    if (m->marker == JPEG_APP0 + 1) {
      buf = (unsigned char *)malloc(m->data_length ? m->data_length : 1);
      if (buf == NULL) {
        jpeg_destroy_decompress(&cinfo);
        return 2;
      }
      for (unsigned int i = 0; i < m->data_length; i++) {
        buf[i] = m->data[i];
      }
      *out = buf;
      *out_len = (size_t)m->data_length;
      break;
    }
  }
  jpeg_destroy_decompress(&cinfo);
  return 0;
}

/* oracle_jpeg_encode plus embedded metadata: an optional raw APP1 payload (written verbatim via
 * jpeg_write_marker, e.g. "Exif\0\0" + TIFF) and an optional ICC profile (written via
 * jpeg_write_icc_profile, which produces the APP2 ICC_PROFILE chunk sequence).
 *
 * Returns 0 on success (and fills a malloc'd `*out`), 1 on a libjpeg error. */
int oracle_jpeg_encode_meta(const unsigned char *pixels, unsigned int width, unsigned int height,
                            int gray, int quality, int h_samp, int v_samp, int progressive,
                            unsigned int restart_interval, int optimize_coding,
                            const unsigned char *app1, size_t app1_len, const unsigned char *icc,
                            size_t icc_len, unsigned char **out, size_t *out_len) {
  struct jpeg_compress_struct cinfo;
  struct oracle_error_mgr jerr;
  unsigned char *volatile outbuf = NULL;
  unsigned long outsize = 0;

  cinfo.err = jpeg_std_error(&jerr.pub);
  jerr.pub.error_exit = oracle_error_exit;
  if (setjmp(jerr.setjmp_buffer)) {
    jpeg_destroy_compress(&cinfo);
    free(outbuf);
    return 1;
  }

  jpeg_create_compress(&cinfo);
  jpeg_mem_dest(&cinfo, (unsigned char **)&outbuf, &outsize);

  cinfo.image_width = width;
  cinfo.image_height = height;
  if (gray) {
    cinfo.input_components = 1;
    cinfo.in_color_space = JCS_GRAYSCALE;
  } else {
    cinfo.input_components = 3;
    cinfo.in_color_space = JCS_RGB;
  }

  jpeg_set_defaults(&cinfo);
  jpeg_set_quality(&cinfo, quality, TRUE);

  if (!gray) {
    cinfo.comp_info[0].h_samp_factor = h_samp;
    cinfo.comp_info[0].v_samp_factor = v_samp;
    cinfo.comp_info[1].h_samp_factor = 1;
    cinfo.comp_info[1].v_samp_factor = 1;
    cinfo.comp_info[2].h_samp_factor = 1;
    cinfo.comp_info[2].v_samp_factor = 1;
  }

  if (progressive) {
    jpeg_simple_progression(&cinfo);
  }
  cinfo.restart_interval = restart_interval;
  cinfo.optimize_coding = optimize_coding ? TRUE : FALSE;

  jpeg_start_compress(&cinfo, TRUE);

  /* Markers must be written after jpeg_start_compress and before the first scanline. */
  if (app1 != NULL && app1_len > 0) {
    jpeg_write_marker(&cinfo, JPEG_APP0 + 1, app1, (unsigned int)app1_len);
  }
  if (icc != NULL && icc_len > 0) {
    jpeg_write_icc_profile(&cinfo, icc, (unsigned int)icc_len);
  }

  size_t row_stride = (size_t)width * (unsigned int)cinfo.input_components;
  while (cinfo.next_scanline < cinfo.image_height) {
    JSAMPROW rowptr = (JSAMPROW)(pixels + (size_t)cinfo.next_scanline * row_stride);
    jpeg_write_scanlines(&cinfo, &rowptr, 1);
  }

  jpeg_finish_compress(&cinfo);
  jpeg_destroy_compress(&cinfo);

  *out = outbuf;
  *out_len = (size_t)outsize;
  return 0;
}

/* Encodes interleaved 8-bit `pixels` (RGB when gray==0, grayscale when gray!=0) to a JPEG.
 *
 * `quality` is applied with force_baseline=TRUE. `h_samp`/`v_samp` set the luma sampling factors
 * (chroma stays 1x1): 1,1 = 4:4:4, 2,1 = 4:2:2, 2,2 = 4:2:0. `progressive` selects a default
 * progressive script; `restart_interval` and `optimize_coding` map to the like-named cinfo fields.
 *
 * Returns 0 on success (and fills a malloc'd `*out`), 1 on a libjpeg error. */
int oracle_jpeg_encode(const unsigned char *pixels, unsigned int width, unsigned int height,
                       int gray, int quality, int h_samp, int v_samp, int progressive,
                       unsigned int restart_interval, int optimize_coding, unsigned char **out,
                       size_t *out_len) {
  struct jpeg_compress_struct cinfo;
  struct oracle_error_mgr jerr;
  /* volatile: re-read in the longjmp handler (jpeg_mem_dest writes it through the pointer). */
  unsigned char *volatile outbuf = NULL;
  unsigned long outsize = 0;

  cinfo.err = jpeg_std_error(&jerr.pub);
  jerr.pub.error_exit = oracle_error_exit;
  if (setjmp(jerr.setjmp_buffer)) {
    jpeg_destroy_compress(&cinfo);
    free(outbuf);
    return 1;
  }

  jpeg_create_compress(&cinfo);
  jpeg_mem_dest(&cinfo, (unsigned char **)&outbuf, &outsize);

  cinfo.image_width = width;
  cinfo.image_height = height;
  if (gray) {
    cinfo.input_components = 1;
    cinfo.in_color_space = JCS_GRAYSCALE;
  } else {
    cinfo.input_components = 3;
    cinfo.in_color_space = JCS_RGB;
  }

  jpeg_set_defaults(&cinfo);
  jpeg_set_quality(&cinfo, quality, TRUE);

  if (!gray) {
    cinfo.comp_info[0].h_samp_factor = h_samp;
    cinfo.comp_info[0].v_samp_factor = v_samp;
    cinfo.comp_info[1].h_samp_factor = 1;
    cinfo.comp_info[1].v_samp_factor = 1;
    cinfo.comp_info[2].h_samp_factor = 1;
    cinfo.comp_info[2].v_samp_factor = 1;
  }

  if (progressive) {
    jpeg_simple_progression(&cinfo);
  }
  cinfo.restart_interval = restart_interval;
  cinfo.optimize_coding = optimize_coding ? TRUE : FALSE;

  jpeg_start_compress(&cinfo, TRUE);

  size_t row_stride = (size_t)width * (unsigned int)cinfo.input_components;
  while (cinfo.next_scanline < cinfo.image_height) {
    JSAMPROW rowptr = (JSAMPROW)(pixels + (size_t)cinfo.next_scanline * row_stride);
    jpeg_write_scanlines(&cinfo, &rowptr, 1);
  }

  jpeg_finish_compress(&cinfo);
  jpeg_destroy_compress(&cinfo);

  *out = outbuf;
  *out_len = (size_t)outsize;
  return 0;
}
