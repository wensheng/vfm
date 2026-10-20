//! Bounded content detection for preview rules, without extension lists or libmagic.

// Rust guideline compliant 2026-10-07

use std::io::Read;
use std::path::Path;

/// Bytes inspected per file, matching infer's file probe and limiting hover I/O.
pub const SNIFF_BYTES: usize = 8192;

/// Broad content categories accepted by previewer `kind` selectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Font,
    Executable,
    Binary,
}

impl Kind {
    /// Parses a configured category name.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "text" => Self::Text,
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            "document" => Self::Document,
            "archive" => Self::Archive,
            "font" => Self::Font,
            "executable" => Self::Executable,
            "binary" => Self::Binary,
            _ => return None,
        })
    }
}

/// Detected category and MIME type; neither borrows the inspected bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileType {
    pub kind: Kind,
    pub mime: &'static str,
}

/// Inspects a regular file's head, following symlinks but skipping special files.
///
/// Returns `None` when metadata or reading fails, or for non-regular files.
pub fn detect_path(path: &Path) -> Option<FileType> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let f = std::fs::File::open(path).ok()?;
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    f.take(SNIFF_BYTES as u64).read_to_end(&mut head).ok()?;
    Some(detect(&head))
}

/// Classifies signatures first, then treats unrecognized non-binary data as text.
pub fn detect(data: &[u8]) -> FileType {
    use infer::MatcherType;

    let head = &data[..data.len().min(SNIFF_BYTES)];
    if let Some(ty) = infer::get(head) {
        if ty.matcher_type() == MatcherType::Text && looks_binary(head) {
            return FileType {
                kind: Kind::Binary,
                mime: "application/octet-stream",
            };
        }
        let mime = ty.mime_type();
        let kind = match ty.matcher_type() {
            MatcherType::Image => Kind::Image,
            MatcherType::Video => Kind::Video,
            MatcherType::Audio => Kind::Audio,
            MatcherType::Doc | MatcherType::Book => Kind::Document,
            // infer groups PDF and RTF with archives; previewers treat them as documents.
            MatcherType::Archive if matches!(mime, "application/pdf" | "application/rtf") => {
                Kind::Document
            }
            MatcherType::Archive => Kind::Archive,
            MatcherType::Font => Kind::Font,
            // infer's application group also contains certificates and disk images.
            MatcherType::App if mime == "application/x-x509-ca-cert" => {
                if looks_binary(head) {
                    Kind::Binary
                } else {
                    Kind::Text
                }
            }
            MatcherType::App if mime == "application/x-qemu-disk" => Kind::Binary,
            MatcherType::App => Kind::Executable,
            MatcherType::Text => Kind::Text,
            _ => Kind::Binary,
        };
        // Without cfb, infer calls every OLE container a Word document. Keep
        // the broad document category, but don't claim a specific Office format.
        let mime = if head.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
            "application/x-ole-storage"
        } else {
            mime
        };
        return FileType { kind, mime };
    }
    if looks_binary(head) {
        FileType {
            kind: Kind::Binary,
            mime: "application/octet-stream",
        }
    } else {
        FileType {
            kind: Kind::Text,
            mime: "text/plain",
        }
    }
}

/// Detects NUL bytes or more than 10% control bytes in the bounded head.
pub fn looks_binary(data: &[u8]) -> bool {
    let head = &data[..data.len().min(SNIFF_BYTES)];
    if head.contains(&0) {
        return true;
    }
    let ctrl = head
        .iter()
        .filter(|&&b| b < 0x09 || (0x0e..0x20).contains(&b))
        .count();
    ctrl * 10 > head.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_identify_media_and_documents_without_filenames() {
        for (bytes, kind, mime) in [
            (&b"\x89PNG\r\n\x1a\n"[..], Kind::Image, "image/png"),
            (&b"GIF89a"[..], Kind::Image, "image/gif"),
            (
                &b"\0\0\0\x18ftypmp42\0\0\0\0mp42isom"[..],
                Kind::Video,
                "video/mp4",
            ),
            (&b"fLaC\0\0\0\0"[..], Kind::Audio, "audio/x-flac"),
            (&b"%PDF-1.7\n"[..], Kind::Document, "application/pdf"),
            (&b"{\\rtf1 hello}"[..], Kind::Document, "application/rtf"),
            (&b"PK\x03\x04\0\0\0\0"[..], Kind::Archive, "application/zip"),
            (
                &b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"[..],
                Kind::Document,
                "application/x-ole-storage",
            ),
        ] {
            assert_eq!(detect(bytes), FileType { kind, mime });
        }
    }

    #[test]
    fn arbitrary_text_and_empty_files_use_the_text_fallback() {
        for bytes in [
            &b""[..],
            b"fn main() {}\n",
            b"{\"a\": 1}\n",
            "hello 世界\n".as_bytes(),
        ] {
            assert_eq!(
                detect(bytes),
                FileType {
                    kind: Kind::Text,
                    mime: "text/plain"
                }
            );
        }
        assert_eq!(
            detect(b"<!DOCTYPE html>\n<html>hello</html>").mime,
            "text/html"
        );
        assert_eq!(detect(b"\0binary").kind, Kind::Binary);
        assert_eq!(detect(b"#!/bin/sh\n\0binary").kind, Kind::Binary);
        assert_eq!(
            detect(b"#!/bin/sh\n\0binary").mime,
            "application/octet-stream"
        );
    }

    #[test]
    fn inspection_is_bounded() {
        let mut bytes = vec![b'a'; SNIFF_BYTES];
        bytes.push(0);
        assert_eq!(detect(&bytes).kind, Kind::Text);
    }

    #[test]
    fn zip_based_books_and_office_files_keep_their_document_types() {
        // ZIP local file headers are 30 bytes; infer examines their first entry.
        let header = b"PK\x03\x04";
        for (entry, mime) in [
            (&b"mimetypeapplication/epub+zip"[..], "application/epub+zip"),
            (
                &b"word/document.xml"[..],
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                &b"xl/workbook.xml"[..],
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                &b"ppt/presentation.xml"[..],
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
        ] {
            let mut data = vec![0; 30];
            data[..header.len()].copy_from_slice(header);
            data.extend_from_slice(entry);
            assert_eq!(
                detect(&data),
                FileType {
                    kind: Kind::Document,
                    mime
                }
            );
        }
    }

    #[test]
    fn text_certificates_are_text_instead_of_executables() {
        let ty = detect(b"-----BEGIN CERTIFICATE-----\nYWJj\n-----END CERTIFICATE-----\n");
        assert_eq!(ty.kind, Kind::Text);
    }

    #[test]
    fn paths_skip_directories_missing_files_and_pipes() {
        let d = std::env::temp_dir().join(format!("vfm-filetype-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("no-extension"), b"hello\n").unwrap();
        assert_eq!(
            detect_path(&d.join("no-extension")).unwrap().kind,
            Kind::Text
        );
        assert_eq!(detect_path(&d), None);
        assert_eq!(detect_path(&d.join("missing")), None);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("no-extension", d.join("link")).unwrap();
            assert_eq!(detect_path(&d.join("link")).unwrap().kind, Kind::Text);
            let pipe = d.join("pipe");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&pipe)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(detect_path(&pipe), None);
        }
        std::fs::remove_dir_all(d).unwrap();
    }
}
