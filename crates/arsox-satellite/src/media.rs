// Copyright © 2026 Jalapeno Labs

//! What kind of file something is, from its bytes first and its name second.
//!
//! Two callers ask, and they trust the answer differently. A turn attachment is
//! handed to a model as an image or a document only when its **bytes** say it
//! is one, because a model API refuses a PNG that is really a ZIP and fails the
//! whole request over it, and a name is whatever the uploader typed. An
//! artifact's content type is a best-effort label for a download, so its name is
//! consulted when its bytes say nothing.
//!
//! The magic numbers are a short table rather than a dependency, because the
//! formats that matter here are few and each signature is a published constant:
//! the four image formats both harnesses take, PDF, and the two 3D formats an
//! agent is most likely to leave in artifacts/.

/// How many leading bytes [`sniff`] needs to recognize everything it knows.
///
/// The longest signature is WebP's: `RIFF`, a four byte size, then `WEBP`.
pub const SNIFF_BYTES: usize = 12;

/// Formats recognized by their leading bytes, and the media type each is.
///
/// Each signature is the format's own published one. glTF binary opens with the
/// ASCII magic `glTF`, and an uncompressed `.blend` with `BLENDER`; a
/// compressed `.blend` opens with a gzip or zstd frame instead and is left to
/// its extension.
const SIGNATURES: [(&[u8], &str); 7] = [
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"\xff\xd8\xff", "image/jpeg"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
    (b"%PDF-", "application/pdf"),
    (b"glTF", "model/gltf-binary"),
    (b"BLENDER", "application/x-blender"),
];

/// Extensions the general table gets wrong or does not know, for 3D assets.
///
/// `mime_guess` has `.glb` and `.gltf` right. It knows nothing of `.blend`, and
/// it maps `.obj` and `.stl` to unrelated formats that happen to share them, a
/// Tgif drawing and a certificate trust list, which is the wrong answer for a
/// file an agent modelling something just wrote.
const EXTENSIONS: [(&str, &str); 6] = [
    ("blend", "application/x-blender"),
    ("obj", "model/obj"),
    ("stl", "model/stl"),
    ("fbx", "application/vnd.autodesk.fbx"),
    ("usdz", "model/vnd.usdz+zip"),
    ("exr", "image/x-exr"),
];

/// The media type a file's leading bytes announce, if they announce one.
#[must_use]
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    // WebP is a RIFF container, so its signature has a size in the middle.
    if head.len() >= SNIFF_BYTES && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return Some("image/webp");
    }

    SIGNATURES
        .iter()
        .find(|(signature, _media_type)| head.starts_with(signature))
        .map(|(_signature, media_type)| *media_type)
}

/// A best-effort content type: the bytes first, then the name.
///
/// `None` when neither says anything, which is a different statement from
/// `application/octet-stream`: the contract's `content_type` is absent when the
/// satellite could not tell, never a guess dressed as an answer.
#[must_use]
pub fn content_type(head: &[u8], name: &str) -> Option<String> {
    if let Some(sniffed) = sniff(head) {
        return Some(sniffed.to_owned());
    }

    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)?;

    if let Some((_extension, media_type)) = EXTENSIONS
        .iter()
        .find(|(known, _media_type)| *known == extension)
    {
        return Some((*media_type).to_owned());
    }

    mime_guess::from_ext(&extension)
        .first()
        .map(|guessed| guessed.essence_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_image_format_a_harness_takes_is_recognized_by_its_bytes() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), Some("image/png"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0\0\x10JFIF"), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a\x01\0\x01\0"), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\x24\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"%PDF-1.7\n"), Some("application/pdf"));
    }

    #[test]
    fn a_riff_container_that_is_not_webp_is_not_an_image() {
        // A WAV file is RIFF too, and handing one to a model as an image fails
        // the whole request.
        assert_eq!(sniff(b"RIFF\x24\0\0\0WAVEfmt "), None);
    }

    #[test]
    fn a_name_never_makes_bytes_an_image() {
        assert_eq!(sniff(b"PK\x03\x04 a zip"), None);
        assert_eq!(
            content_type(b"PK\x03\x04", "archive.zip").as_deref(),
            Some("application/zip")
        );
    }

    #[test]
    fn models_and_scenes_are_labelled_from_their_bytes_or_their_extension() {
        assert_eq!(
            content_type(b"glTF\x02\0\0\0", "model.bin").as_deref(),
            Some("model/gltf-binary")
        );
        assert_eq!(
            content_type(b"BLENDER-v404", "scene.anything").as_deref(),
            Some("application/x-blender")
        );
        // A compressed .blend opens with a zstd frame, so its name decides.
        assert_eq!(
            content_type(b"\x28\xb5\x2f\xfd", "scene.BLEND").as_deref(),
            Some("application/x-blender")
        );
        assert_eq!(content_type(b"", "mesh.obj").as_deref(), Some("model/obj"));
        assert_eq!(
            content_type(b"", "model.glb").as_deref(),
            Some("model/gltf-binary")
        );
    }

    #[test]
    fn a_file_nothing_recognizes_has_no_content_type() {
        assert_eq!(content_type(b"\0\x01\x02", "mystery"), None);
        assert_eq!(content_type(b"\0\x01\x02", "mystery.qqqz"), None);
    }
}
