// A tiny `extern "C"` bridge over exiv2's XMP parser/serializer (the bundled Adobe XMPCore), its
// bare-TIFF EXIF parser, and its container-level `ImageFactory` reader/writer. Every input is an
// in-memory buffer; exiv2 throws on error, so every entry point is wrapped in try/catch and
// reports failure through a non-zero return code (the container-level calls also record a message
// for `exiv2_last_error`). Buffers handed back to Rust are malloc'd and freed via `exiv2_free`.

#include <exiv2/exiv2.hpp>

#include <cstdlib>
#include <cstring>
#include <string>

namespace {
// Silence exiv2's warning/error logging at load time: the oracle reports failures through return
// codes, so its negative tests would otherwise spew XMP-toolkit diagnostics to stderr.
struct MuteLogging {
    MuteLogging() {
        Exiv2::LogMsg::setLevel(Exiv2::LogMsg::mute);
    }
};
const MuteLogging mute_logging;

// Copies `s` into a malloc'd, NUL-terminated buffer; `*out_len` receives the length (excluding NUL).
char* dup_bytes(const std::string& s, size_t* out_len) {
    char* p = static_cast<char*>(std::malloc(s.size() + 1));
    if (p == nullptr) {
        return nullptr;
    }
    std::memcpy(p, s.data(), s.size());
    p[s.size()] = '\0';
    if (out_len != nullptr) {
        *out_len = s.size();
    }
    return p;
}

// The message of the last failing `exiv2_image_*` call, read back through `exiv2_last_error`.
// Unsynchronized on purpose: the Rust side holds one global lock across each call and its read.
std::string g_last_error;

// Opens an image file held in memory (exiv2 copies it into a MemIo) and reads its metadata.
// Throws Exiv2::Error on an unknown or malformed container.
Exiv2::Image::UniquePtr open_image(const char* data, size_t len) {
    auto image = Exiv2::ImageFactory::open(reinterpret_cast<const Exiv2::byte*>(data), len);
    if (!image) {
        throw Exiv2::Error(Exiv2::ErrorCode::kerMemoryContainsUnknownImageType);
    }
    image->readMetadata();
    return image;
}
} // namespace

extern "C" {

// Returns 0 if exiv2 parses the packet, non-zero otherwise.
int exiv2_xmp_validate(const char* xmp, size_t len) {
    try {
        Exiv2::XmpData data;
        std::string packet(xmp, len);
        return Exiv2::XmpParser::decode(data, packet) == 0 ? 0 : 1;
    } catch (...) {
        return 1;
    }
}

// Parses then re-serializes the packet via XMPCore; `*out_buf` receives a malloc'd UTF-8 string.
int exiv2_xmp_roundtrip(const char* xmp, size_t len, char** out_buf, size_t* out_len) {
    try {
        Exiv2::XmpData data;
        std::string packet(xmp, len);
        if (Exiv2::XmpParser::decode(data, packet) != 0) {
            return 1;
        }
        std::string out;
        if (Exiv2::XmpParser::encode(out, data) != 0) {
            return 2;
        }
        *out_buf = dup_bytes(out, out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (...) {
        return 1;
    }
}

// Reads one property's serialized value by key (e.g. "Xmp.dc.format"); returns 4 if absent.
int exiv2_xmp_get(const char* xmp, size_t len, const char* key, char** out_buf, size_t* out_len) {
    try {
        Exiv2::XmpData data;
        std::string packet(xmp, len);
        if (Exiv2::XmpParser::decode(data, packet) != 0) {
            return 1;
        }
        auto pos = data.findKey(Exiv2::XmpKey(key));
        if (pos == data.end()) {
            return 4;
        }
        *out_buf = dup_bytes(pos->toString(), out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (...) {
        return 1;
    }
}

// Writes the number of parsed XMP properties to `*out_count`.
int exiv2_xmp_count(const char* xmp, size_t len, size_t* out_count) {
    try {
        Exiv2::XmpData data;
        std::string packet(xmp, len);
        if (Exiv2::XmpParser::decode(data, packet) != 0) {
            return 1;
        }
        *out_count = static_cast<size_t>(data.count());
        return 0;
    } catch (...) {
        return 1;
    }
}

// ---- EXIF: a bare TIFF stream (no "Exif\0\0" marker), via Exiv2::ExifParser. -------------------

// Decodes a TIFF-format EXIF buffer and writes the number of tags exiv2 read to `*out_count`.
int exiv2_exif_count(const char* data, size_t len, size_t* out_count) {
    try {
        Exiv2::ExifData exifData;
        Exiv2::ExifParser::decode(
            exifData, reinterpret_cast<const Exiv2::byte*>(data), len);
        *out_count = static_cast<size_t>(exifData.count());
        return 0;
    } catch (...) {
        return 1;
    }
}

// Reads one tag's serialized value by exiv2 key (e.g. "Exif.Image.Make"); returns 4 if absent.
int exiv2_exif_get(const char* data, size_t len, const char* key, char** out_buf, size_t* out_len) {
    try {
        Exiv2::ExifData exifData;
        Exiv2::ExifParser::decode(
            exifData, reinterpret_cast<const Exiv2::byte*>(data), len);
        auto pos = exifData.findKey(Exiv2::ExifKey(key));
        if (pos == exifData.end()) {
            return 4;
        }
        *out_buf = dup_bytes(pos->toString(), out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (...) {
        return 1;
    }
}

// Reads one tag's *interpreted* value by exiv2 key — `Exifdatum::print`, exiv2's human-readable
// rendering (`Exif.Image.Orientation` = 1 prints as "top, left"), as opposed to `toString`'s raw
// value. A value exiv2 has no interpretation for prints as its raw value in parentheses ("(9)").
// Returns 4 if the tag is absent.
int exiv2_exif_print(const char* data, size_t len, const char* key, char** out_buf, size_t* out_len) {
    try {
        Exiv2::ExifData exifData;
        Exiv2::ExifParser::decode(
            exifData, reinterpret_cast<const Exiv2::byte*>(data), len);
        auto pos = exifData.findKey(Exiv2::ExifKey(key));
        if (pos == exifData.end()) {
            return 4;
        }
        *out_buf = dup_bytes(pos->print(&exifData), out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (...) {
        return 1;
    }
}

// Decodes then re-encodes the EXIF buffer via exiv2, returning its canonical TIFF bytes in
// `*out_buf` (a fresh bare TIFF stream in the decoded byte order).
int exiv2_exif_roundtrip(const char* data, size_t len, char** out_buf, size_t* out_len) {
    try {
        Exiv2::ExifData exifData;
        Exiv2::ByteOrder order = Exiv2::ExifParser::decode(
            exifData, reinterpret_cast<const Exiv2::byte*>(data), len);
        Exiv2::Blob blob;
        Exiv2::ExifParser::encode(blob, nullptr, 0, order, exifData);
        std::string bytes(reinterpret_cast<const char*>(blob.data()), blob.size());
        *out_buf = dup_bytes(bytes, out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (...) {
        return 1;
    }
}

// ---- Container level: a whole image file (PNG/WebP/TIFF/BMFF...) via Exiv2::ImageFactory. -----
//
// Each entry point opens the image from memory (exiv2 copies it into a MemIo), runs
// `readMetadata`, and works on the result. Return codes, shared by all four:
//   0 = ok, 1 = exiv2 threw (message via `exiv2_last_error`), 3 = allocation failure,
//   4 = key absent, 5 = image carries no ICC profile (`exiv2_image_icc` only),
//   6 = exiv2 cannot write that metadata kind into this format (`exiv2_image_set` only).

// Reads one Exif tag's `toString()` by key (e.g. "Exif.Photo.ISOSpeedRatings").
int exiv2_image_exif_get(const char* data, size_t len, const char* key, char** out_buf, size_t* out_len) {
    try {
        auto image = open_image(data, len);
        auto& exifData = image->exifData();
        auto pos = exifData.findKey(Exiv2::ExifKey(key));
        if (pos == exifData.end()) {
            g_last_error = std::string("Exif key absent: ") + key;
            return 4;
        }
        *out_buf = dup_bytes(pos->toString(), out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (const std::exception& e) {
        g_last_error = e.what();
        return 1;
    } catch (...) {
        g_last_error = "unknown exception";
        return 1;
    }
}

// Reads one XMP property's `toString()` by key (e.g. "Xmp.xmp.CreatorTool").
int exiv2_image_xmp_get(const char* data, size_t len, const char* key, char** out_buf, size_t* out_len) {
    try {
        auto image = open_image(data, len);
        auto& xmpData = image->xmpData();
        auto pos = xmpData.findKey(Exiv2::XmpKey(key));
        if (pos == xmpData.end()) {
            g_last_error = std::string("XMP key absent: ") + key;
            return 4;
        }
        *out_buf = dup_bytes(pos->toString(), out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (const std::exception& e) {
        g_last_error = e.what();
        return 1;
    } catch (...) {
        g_last_error = "unknown exception";
        return 1;
    }
}

// Copies the raw ICC profile exiv2 read from the container; returns 5 (and leaves `*out_buf`
// untouched) when the image carries none.
int exiv2_image_icc(const char* data, size_t len, char** out_buf, size_t* out_len) {
    try {
        auto image = open_image(data, len);
        if (!image->iccProfileDefined()) {
            g_last_error = "image carries no ICC profile";
            return 5;
        }
        const Exiv2::DataBuf& icc = image->iccProfile();
        *out_buf = dup_bytes(std::string(icc.c_str(), icc.size()), out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (const std::exception& e) {
        g_last_error = e.what();
        return 1;
    } catch (...) {
        g_last_error = "unknown exception";
        return 1;
    }
}

// Sets `exif_key` = `exif_value` and/or `xmp_key` = `xmp_value` (each pair skipped when its key is
// NULL), runs `writeMetadata`, and returns the whole rewritten file in `*out_buf`. Returns 6 when
// exiv2's format table does not mark the requested metadata kind read-write for this format
// (BMFF is read-only throughout).
int exiv2_image_set(const char* data, size_t len, const char* exif_key, const char* exif_value,
                    const char* xmp_key, const char* xmp_value, char** out_buf, size_t* out_len) {
    try {
        auto image = open_image(data, len);
        const auto type = image->imageType();
        if (exif_key != nullptr &&
            Exiv2::ImageFactory::checkMode(type, Exiv2::mdExif) != Exiv2::amReadWrite) {
            g_last_error = "exiv2 cannot write Exif into this image format";
            return 6;
        }
        if (xmp_key != nullptr &&
            Exiv2::ImageFactory::checkMode(type, Exiv2::mdXmp) != Exiv2::amReadWrite) {
            g_last_error = "exiv2 cannot write XMP into this image format";
            return 6;
        }
        if (exif_key != nullptr) {
            image->exifData()[exif_key] = std::string(exif_value != nullptr ? exif_value : "");
        }
        if (xmp_key != nullptr) {
            image->xmpData()[xmp_key] = std::string(xmp_value != nullptr ? xmp_value : "");
        }
        image->writeMetadata();
        Exiv2::BasicIo& io = image->io();
        if (io.open() != 0) {
            g_last_error = "could not reopen the rewritten image";
            return 1;
        }
        io.seek(0, Exiv2::BasicIo::beg);
        Exiv2::DataBuf bytes = io.read(io.size());
        io.close();
        *out_buf = dup_bytes(std::string(bytes.c_str(), bytes.size()), out_len);
        return *out_buf != nullptr ? 0 : 3;
    } catch (const std::exception& e) {
        g_last_error = e.what();
        return 1;
    } catch (...) {
        g_last_error = "unknown exception";
        return 1;
    }
}

// Copies the message the last failing `exiv2_image_*` call recorded into a malloc'd string
// (freed via `exiv2_free`). Callers serialize every exiv2 call, so "last" is well defined.
int exiv2_last_error(char** out_buf, size_t* out_len) {
    *out_buf = dup_bytes(g_last_error, out_len);
    return *out_buf != nullptr ? 0 : 3;
}

void exiv2_free(char* p) {
    std::free(p);
}

} // extern "C"
