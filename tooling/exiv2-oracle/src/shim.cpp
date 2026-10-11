// A tiny `extern "C"` bridge over exiv2's XMP parser/serializer (the bundled Adobe XMPCore).
// Each function decodes an in-memory XMP packet; exiv2 throws on error, so every entry point is
// wrapped in try/catch and reports failure through a non-zero return code. Strings handed back to
// Rust are malloc'd and freed via `exiv2_free`.

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

void exiv2_free(char* p) {
    std::free(p);
}

} // extern "C"
