// Copyright © 2026 Jalapeno Labs

//! Files that come with a turn's prompt.
//!
//! The host uploads a file into the thread's workspace with the file routes and
//! names it in `StartTurnRequest.attachments`. Two moments matter, and the
//! workspace belongs to the agent in between, so both read the file through
//! [`crate::workspace::confined`] and neither trusts the other:
//!
//! - **Submission.** [`admit`] refuses a turn whose attachments break a rule
//!   while the caller is still listening, and records what it found: the size
//!   and, from the file's first bytes, the media type. What the caller claimed
//!   about either is ignored, as the contract says.
//! - **Spawn.** [`deliver`] reads every file again when the harness starts,
//!   because the agent can replace one with a link, a directory, or something a
//!   thousand times larger between the two. A file that no longer passes is
//!   named in the prompt as one that could not be read, and the caller records a
//!   degraded incident, rather than failing a turn over a picture.
//!
//! # What each harness is handed
//!
//! | Bytes say | Claude | Codex |
//! |---|---|---|
//! | PNG, JPEG, GIF, WebP | an `image` block on stdin | `-i <path>` |
//! | PDF | a `document` block on stdin | named in the prompt |
//! | anything else | named in the prompt | named in the prompt |
//!
//! The bytes decide, never the name: a model API refuses a "PNG" that is really
//! a ZIP and fails the whole request over it.
//!
//! # Why these caps
//!
//! 3.75 MiB a file is the largest raw file whose base64 fits the 5 MB a model
//! API accepts per image, so nothing admitted here is refused further down. 8
//! files and 12 MiB together keep one prompt, base64 included, well inside what
//! a single model request carries.

use crate::workspace::confined::{self, FileError};
use arsox_sdk::proto::turn::v1::TurnAttachment;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// The most files one turn may carry.
pub const MAX_ATTACHMENTS: usize = 8;

/// The largest one file may be: 3.75 MiB. See the module docs.
pub const MAX_ATTACHMENT_BYTES: u64 = 3 * 1024 * 1024 + 768 * 1024;

/// The largest the files of one turn may be together.
pub const MAX_TOTAL_BYTES: u64 = 12 * 1024 * 1024;

/// Media types both harnesses take as an image.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The media type Claude takes as a document.
const PDF_TYPE: &str = "application/pdf";

/// Why a turn's attachments were refused at submission.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// One file's path, or what sits at it, broke the workspace file rules.
    #[error("attachments[{index}] {path:?}: {error}")]
    File {
        index: usize,
        path: String,
        error: FileError,
    },

    /// A count or a size cap. The reason names the field.
    #[error("{0}")]
    Limit(String),
}

/// Checks a submission's attachments and records what was found at each path.
///
/// Returns the attachments as the satellite will store them: the paths as
/// sent, in order, each with its measured size and sniffed media type.
///
/// # Errors
///
/// [`Refusal::Limit`] past a count or size cap, and [`Refusal::File`] for a
/// path the file routes would refuse or one with nothing regular at it.
pub async fn admit(
    thread_directory: PathBuf,
    requested: Vec<TurnAttachment>,
) -> Result<Vec<TurnAttachment>, Refusal> {
    if requested.len() > MAX_ATTACHMENTS {
        return Err(Refusal::Limit(format!(
            "attachments: a turn carries at most {MAX_ATTACHMENTS} files, and this one named {}",
            requested.len()
        )));
    }

    // Every open below is a blocking system call, and a walk down a deep path
    // is several of them.
    tokio::task::spawn_blocking(move || admit_blocking(&thread_directory, requested))
        .await
        .unwrap_or_else(|panicked| {
            Err(Refusal::File {
                index: 0,
                path: String::new(),
                error: FileError::Io(std::io::Error::other(panicked)),
            })
        })
}

fn admit_blocking(
    thread_directory: &Path,
    requested: Vec<TurnAttachment>,
) -> Result<Vec<TurnAttachment>, Refusal> {
    let mut admitted = Vec::with_capacity(requested.len());
    let mut total: u64 = 0;

    for (index, attachment) in requested.into_iter().enumerate() {
        let refused = |error| Refusal::File {
            index,
            path: attachment.path.clone(),
            error,
        };

        let pieces = confined::owned_components(&attachment.path).map_err(refused)?;
        let (mut file, size) = confined::open_file(thread_directory, &pieces).map_err(refused)?;

        if size > MAX_ATTACHMENT_BYTES {
            return Err(Refusal::Limit(format!(
                "attachments[{index}] {:?} is {size} bytes, past the {MAX_ATTACHMENT_BYTES} \
                 bytes one attachment may be",
                attachment.path
            )));
        }

        total += size;
        if total > MAX_TOTAL_BYTES {
            return Err(Refusal::Limit(format!(
                "attachments: together they are past the {MAX_TOTAL_BYTES} bytes one turn's \
                 attachments may be"
            )));
        }

        let mut head = Vec::with_capacity(crate::media::SNIFF_BYTES);
        file.by_ref()
            .take(crate::media::SNIFF_BYTES as u64)
            .read_to_end(&mut head)
            .map_err(|error| refused(FileError::Io(error)))?;

        admitted.push(TurnAttachment {
            path: attachment.path,
            content_type: crate::media::sniff(&head).map(str::to_owned),
            size_bytes: size,
        });
    }

    Ok(admitted)
}

/// One attachment as it stood when the harness started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    /// Relative to the thread's workspace, as submitted.
    pub path: String,

    /// Where the file is on disk, for a harness that reads it itself.
    pub absolute: PathBuf,

    pub content: Content,
}

/// What a delivered attachment turned out to be.
#[derive(Clone, PartialEq, Eq)]
pub enum Content {
    /// An image both harnesses take natively.
    Image {
        media_type: &'static str,
        bytes: Vec<u8>,
    },

    /// A PDF, which Claude takes as a document.
    Pdf { bytes: Vec<u8> },

    /// Anything else, named in the prompt for the agent to open.
    Other {
        media_type: Option<&'static str>,
        size_bytes: u64,
    },

    /// The file no longer passes the rules it was admitted under. The reason
    /// completes "it could not be read because ...".
    Unreadable(String),
}

/// Written by hand so a log line formatting a command never carries megabytes
/// of image bytes.
impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Image { media_type, bytes } => {
                write!(f, "Image({media_type}, {} bytes)", bytes.len())
            }
            Self::Pdf { bytes } => write!(f, "Pdf({} bytes)", bytes.len()),
            Self::Other {
                media_type,
                size_bytes,
            } => write!(f, "Other({media_type:?}, {size_bytes} bytes)"),
            Self::Unreadable(reason) => write!(f, "Unreadable({reason:?})"),
        }
    }
}

impl Delivered {
    /// Why this attachment could not be handed over, when it could not.
    #[must_use]
    pub fn unreadable(&self) -> Option<&str> {
        match &self.content {
            Content::Unreadable(reason) => Some(reason),
            _delivered => None,
        }
    }
}

/// Reads a claimed turn's attachments again, as the harness starts.
///
/// Never fails: a file that no longer passes is [`Content::Unreadable`], and
/// the caller records it. See the module docs.
pub async fn deliver(
    thread_directory: PathBuf,
    attachments: Vec<TurnAttachment>,
) -> Vec<Delivered> {
    if attachments.is_empty() {
        return Vec::new();
    }

    let fallback = attachments.clone();
    let directory = thread_directory.clone();

    tokio::task::spawn_blocking(move || deliver_blocking(&directory, attachments))
        .await
        .unwrap_or_else(|panicked| {
            fallback
                .into_iter()
                .map(|attachment| Delivered {
                    absolute: thread_directory.join(&attachment.path),
                    path: attachment.path,
                    content: Content::Unreadable(format!("reading it failed: {panicked}")),
                })
                .collect()
        })
}

fn deliver_blocking(thread_directory: &Path, attachments: Vec<TurnAttachment>) -> Vec<Delivered> {
    let mut total: u64 = 0;

    attachments
        .into_iter()
        .map(|attachment| {
            let content = match read_bounded(thread_directory, &attachment.path, &mut total) {
                Ok(bytes) => classify(bytes),
                Err(reason) => Content::Unreadable(reason),
            };

            Delivered {
                absolute: thread_directory.join(&attachment.path),
                path: attachment.path,
                content,
            }
        })
        .collect()
}

/// Reads one attachment whole, refusing anything past the caps it was
/// admitted under.
fn read_bounded(thread_directory: &Path, path: &str, total: &mut u64) -> Result<Vec<u8>, String> {
    let pieces = confined::owned_components(path).map_err(|error| error.to_string())?;
    let (file, size) =
        confined::open_file(thread_directory, &pieces).map_err(|error| error.to_string())?;

    if size > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "it grew to {size} bytes after the turn was queued, past the \
             {MAX_ATTACHMENT_BYTES} one attachment may be"
        ));
    }

    *total += size;
    if *total > MAX_TOTAL_BYTES {
        return Err(format!(
            "the turn's attachments grew past {MAX_TOTAL_BYTES} bytes together after it was queued"
        ));
    }

    // Bounded by the size measured on the open handle, so a file the agent
    // appends to while it is read is still read to the size that was checked.
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
    file.take(size)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading it failed: {error}"))?;

    Ok(bytes)
}

/// Decides from its bytes what an attachment is.
fn classify(bytes: Vec<u8>) -> Content {
    match crate::media::sniff(&bytes) {
        Some(media_type) if IMAGE_TYPES.contains(&media_type) => {
            Content::Image { media_type, bytes }
        }
        Some(PDF_TYPE) => Content::Pdf { bytes },
        media_type => Content::Other {
            media_type,
            size_bytes: bytes.len() as u64,
        },
    }
}

/// The paragraph that names, in the prompt, what a harness was not handed.
///
/// `handed` says whether this harness took an attachment natively. `None` when
/// it took every one, so a prompt with nothing to add is left as written.
#[must_use]
pub fn prompt_note(attachments: &[Delivered], handed: impl Fn(&Content) -> bool) -> Option<String> {
    let lines: Vec<String> = attachments
        .iter()
        .filter(|attachment| !handed(&attachment.content))
        .map(|attachment| match &attachment.content {
            Content::Unreadable(reason) => {
                format!("- `{}` could not be read: {reason}", attachment.path)
            }
            Content::Pdf { bytes } => {
                format!(
                    "- `{}` ({PDF_TYPE}, {} bytes)",
                    attachment.path,
                    bytes.len()
                )
            }
            Content::Image { media_type, bytes } => {
                format!(
                    "- `{}` ({media_type}, {} bytes)",
                    attachment.path,
                    bytes.len()
                )
            }
            Content::Other {
                media_type,
                size_bytes,
            } => format!(
                "- `{}` ({}, {size_bytes} bytes)",
                attachment.path,
                media_type.unwrap_or("unrecognized type")
            ),
        })
        .collect();

    if lines.is_empty() {
        return None;
    }

    Some(format!(
        "Files attached to this message, in the workspace relative to its root. Open them to \
         read them:\n{}",
        lines.join("\n")
    ))
}

/// The prompt with the note about unhanded attachments after it, if any.
#[must_use]
pub fn prompt_with_note(prompt: &str, note: Option<String>) -> String {
    match note {
        Some(note) => format!("{prompt}\n\n{note}"),
        None => prompt.to_owned(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A scratch thread directory, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("arsox-attachments-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&root).expect("should create the scratch workspace");
            Self(root)
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let target = self.0.join(path);
            std::fs::create_dir_all(target.parent().expect("a file has a parent"))
                .expect("should create the parent");
            std::fs::write(target, bytes).expect("should write the file");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn named(path: &str) -> TurnAttachment {
        TurnAttachment {
            path: path.to_owned(),
            content_type: Some("text/lies".to_owned()),
            size_bytes: 999,
        }
    }

    #[tokio::test]
    async fn admission_records_what_the_bytes_say_and_ignores_the_claim() {
        let scratch = Scratch::new();
        scratch.write("feedback/1/annotated.png", PNG);
        scratch.write("notes.zip", b"PK\x03\x04");

        let admitted = admit(
            scratch.0.clone(),
            vec![named("feedback/1/annotated.png"), named("notes.zip")],
        )
        .await
        .expect("both files are fine");

        assert_eq!(admitted[0].content_type.as_deref(), Some("image/png"));
        assert_eq!(admitted[0].size_bytes, PNG.len() as u64);
        assert_eq!(admitted[1].content_type, None, "a zip is not sniffed");
        assert_eq!(admitted[1].path, "notes.zip");
    }

    #[tokio::test]
    async fn a_path_the_file_routes_refuse_is_refused_here() {
        let scratch = Scratch::new();

        for (path, expected) in [
            ("../escape.png", "InvalidPath"),
            ("/etc/passwd", "InvalidPath"),
            ("missing.png", "NotFound"),
        ] {
            let refusal = admit(scratch.0.clone(), vec![named(path)])
                .await
                .expect_err("the path is not admissible");

            assert!(
                matches!(&refusal, Refusal::File { error, .. } if format!("{error:?}").starts_with(expected)),
                "{path}: {refusal:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_directory_or_a_link_is_not_an_attachment() {
        let scratch = Scratch::new();
        std::fs::create_dir_all(scratch.0.join("folder")).expect("should create the folder");
        std::os::unix::fs::symlink("/etc/passwd", scratch.0.join("link.png"))
            .expect("should create the link");

        let folder = admit(scratch.0.clone(), vec![named("folder")]).await;
        assert!(matches!(
            folder,
            Err(Refusal::File {
                error: FileError::NotRegular,
                ..
            })
        ));

        let link = admit(scratch.0.clone(), vec![named("link.png")]).await;
        assert!(matches!(
            link,
            Err(Refusal::File {
                error: FileError::InvalidPath(_),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn the_count_and_size_caps_are_refused_rather_than_trimmed() {
        let scratch = Scratch::new();
        scratch.write("one.png", PNG);

        let nine = vec![named("one.png"); MAX_ATTACHMENTS + 1];
        assert!(matches!(
            admit(scratch.0.clone(), nine).await,
            Err(Refusal::Limit(_))
        ));

        let big = vec![0_u8; usize::try_from(MAX_ATTACHMENT_BYTES).expect("fits") + 1];
        scratch.write("big.bin", &big);
        assert!(matches!(
            admit(scratch.0.clone(), vec![named("big.bin")]).await,
            Err(Refusal::Limit(_))
        ));

        // Four files under the per-file cap, together past the total.
        let three_and_a_half = vec![0_u8; 3 * 1024 * 1024 + 512 * 1024];
        for index in 0..4 {
            scratch.write(&format!("part-{index}.bin"), &three_and_a_half);
        }
        let parts = (0..4)
            .map(|index| named(&format!("part-{index}.bin")))
            .collect();
        assert!(matches!(
            admit(scratch.0.clone(), parts).await,
            Err(Refusal::Limit(_))
        ));
    }

    #[tokio::test]
    async fn delivery_reads_the_bytes_again_and_survives_a_swapped_file() {
        let scratch = Scratch::new();
        scratch.write("sketch.png", PNG);
        scratch.write("doc.pdf", b"%PDF-1.7\n");
        scratch.write("data.csv", b"a,b\n");
        std::os::unix::fs::symlink("/etc/passwd", scratch.0.join("swapped.png"))
            .expect("should create the link");

        let delivered = deliver(
            scratch.0.clone(),
            ["sketch.png", "doc.pdf", "data.csv", "swapped.png"]
                .into_iter()
                .map(named)
                .collect(),
        )
        .await;

        assert!(matches!(
            delivered[0].content,
            Content::Image {
                media_type: "image/png",
                ..
            }
        ));
        assert!(matches!(delivered[1].content, Content::Pdf { .. }));
        assert!(matches!(delivered[2].content, Content::Other { .. }));
        assert!(
            delivered[3].unreadable().is_some(),
            "a link is never followed"
        );
        assert_eq!(delivered[0].absolute, scratch.0.join("sketch.png"));
    }

    #[test]
    fn the_note_names_only_what_the_harness_was_not_handed() {
        let delivered = vec![
            Delivered {
                path: "sketch.png".to_owned(),
                absolute: PathBuf::from("/w/sketch.png"),
                content: Content::Image {
                    media_type: "image/png",
                    bytes: PNG.to_vec(),
                },
            },
            Delivered {
                path: "doc.pdf".to_owned(),
                absolute: PathBuf::from("/w/doc.pdf"),
                content: Content::Pdf {
                    bytes: b"%PDF-".to_vec(),
                },
            },
        ];

        let images_only = |content: &Content| matches!(content, Content::Image { .. });
        let note = prompt_note(&delivered, images_only).expect("the PDF is named");
        assert!(note.contains("`doc.pdf` (application/pdf, 5 bytes)"));
        assert!(!note.contains("sketch.png"));

        assert_eq!(prompt_note(&delivered, |_content| true), None);
    }
}
