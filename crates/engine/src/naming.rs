//! What a downloaded file is called: the one set of rules for transfers,
//! the early name hint, and link checks.
//!
//! A name comes from the response's `Content-Disposition` (RFC 6266 with
//! RFC 8187 values), else a provider's suggestion, else the name a signed
//! object-storage link asks its server to send, else the link's last path
//! segment. A name without an extension takes the conventional one of a
//! specific response type. Every candidate must be a single valid path
//! component (ADR-17), so no name can choose a directory. Only a derived
//! name is kept, never the query it came from, which may hold a token.

use percent_encoding::percent_decode_str;
use url::Url;
use wayfetch_domain::id::JobId;

use crate::fs::RelativePath;

/// What a `Content-Disposition` header value says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Disposition {
    /// Whether its type is `attachment`.
    pub attachment: bool,
    /// The file name it suggests, not yet validated: `filename*` in UTF-8
    /// if present, else `filename`.
    pub filename: Option<String>,
}

impl Disposition {
    /// Reads a `Content-Disposition` value, or the same value carried in a
    /// signed link's query.
    pub fn parse(value: &str) -> Self {
        Self {
            attachment: params(value)
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("attachment")),
            filename: disposition_filename(value),
        }
    }
}

/// A `Content-Type` value's media type, without parameters, lower case.
pub fn media_type(value: &str) -> Option<String> {
    value
        .split(';')
        .next()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

/// `name` as a single valid path component, which cannot choose a
/// directory.
pub(crate) fn file_name(name: &str) -> Option<RelativePath> {
    if name.contains('/') {
        return None;
    }
    RelativePath::new(name).ok()
}

/// Whether `name` ends in an extension: a dot after a non-empty stem, then
/// one to sixteen ASCII letters or digits.
pub(crate) fn has_extension(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && (1..=16).contains(&extension.len())
            && extension.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// What is known about a file's name, from the strongest source to the
/// weakest. Absent sources are skipped.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Naming<'a> {
    /// The response's `Content-Disposition` file name.
    pub(crate) disposition: Option<&'a str>,
    /// A provider's or file host's suggestion, or a name recorded earlier.
    pub(crate) provider: Option<&'a str>,
    /// The link the bytes come from.
    pub(crate) url: Option<&'a Url>,
    /// The response's media type.
    pub(crate) media_type: Option<&'a str>,
}

impl<'a> Naming<'a> {
    /// What the link alone tells.
    pub(crate) fn of(url: &'a Url) -> Self {
        Self {
            url: Some(url),
            ..Self::default()
        }
    }

    /// The best valid name, if any source gives one.
    pub(crate) fn name(&self) -> Option<RelativePath> {
        let chosen = self
            .disposition
            .and_then(file_name)
            .or_else(|| self.provider.and_then(file_name))
            .or_else(|| self.url.and_then(query_name))
            .or_else(|| self.url.and_then(path_name))?;
        Some(self.typed(chosen))
    }

    /// The best name, else one derived from `job`.
    pub(crate) fn or_job(&self, job: JobId) -> RelativePath {
        self.name().unwrap_or_else(|| {
            self.typed(
                RelativePath::new(&format!("download-{}", job.get())).expect("a valid file name"),
            )
        })
    }

    /// `name` with the media type's extension if it has none and the type
    /// is specific; unchanged if the longer name would not be valid.
    fn typed(&self, name: RelativePath) -> RelativePath {
        if has_extension(name.as_str()) {
            return name;
        }
        self.media_type
            .and_then(type_extension)
            .and_then(|extension| file_name(&format!("{name}.{extension}")))
            .unwrap_or(name)
    }
}

/// Query parameters with which a signed object-storage link sets the
/// response's `Content-Disposition`: S3-compatible stores and Cloud
/// Storage use `response-content-disposition`, Azure Blob SAS `rscd`.
const QUERY_DISPOSITION: [&str; 2] = ["response-content-disposition", "rscd"];

/// The file name a signed link's query asks its server to send. Several
/// such parameters are ambiguous and name nothing, as several headers do.
fn query_name(url: &Url) -> Option<RelativePath> {
    let mut values = url.query()?.split('&').filter_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        QUERY_DISPOSITION
            .contains(&form_decode(key)?.as_str())
            .then_some(value)
    });
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    file_name(&Disposition::parse(&form_decode(value)?).filename?)
}

/// A query component decoded as a form does, refusing bytes that are not
/// UTF-8 rather than replacing them.
fn form_decode(component: &str) -> Option<String> {
    percent_decode_str(&component.replace('+', " "))
        .decode_utf8()
        .ok()
        .map(|decoded| decoded.into_owned())
}

/// The link's last path segment, percent-decoded.
fn path_name(url: &Url) -> Option<RelativePath> {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|segment| percent_decode_str(segment).decode_utf8().ok())
        .and_then(|name| file_name(&name))
}

/// Conventional extensions of specific media types. Types that say
/// nothing about the file (`application/octet-stream`), describe a page
/// rather than what it serves (`text/html`), or are sent for anything
/// (`text/plain`) are absent, so they add nothing. So are executable and
/// script types, whose extension would make a file run when opened.
const TYPE_EXTENSIONS: &[(&str, &str)] = &[
    ("application/epub+zip", "epub"),
    ("application/gzip", "gz"),
    ("application/json", "json"),
    ("application/msword", "doc"),
    ("application/ogg", "ogx"),
    ("application/pdf", "pdf"),
    ("application/rtf", "rtf"),
    ("application/vnd.android.package-archive", "apk"),
    ("application/vnd.debian.binary-package", "deb"),
    ("application/vnd.ms-excel", "xls"),
    ("application/vnd.ms-powerpoint", "ppt"),
    ("application/vnd.oasis.opendocument.presentation", "odp"),
    ("application/vnd.oasis.opendocument.spreadsheet", "ods"),
    ("application/vnd.oasis.opendocument.text", "odt"),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    ("application/vnd.rar", "rar"),
    ("application/x-7z-compressed", "7z"),
    ("application/x-apple-diskimage", "dmg"),
    ("application/x-bittorrent", "torrent"),
    ("application/x-bzip2", "bz2"),
    ("application/x-gzip", "gz"),
    ("application/x-iso9660-image", "iso"),
    ("application/x-rar-compressed", "rar"),
    ("application/x-rpm", "rpm"),
    ("application/x-tar", "tar"),
    ("application/x-xz", "xz"),
    ("application/x-zip-compressed", "zip"),
    ("application/xml", "xml"),
    ("application/zip", "zip"),
    ("application/zstd", "zst"),
    ("audio/aac", "aac"),
    ("audio/flac", "flac"),
    ("audio/mp4", "m4a"),
    ("audio/mpeg", "mp3"),
    ("audio/ogg", "ogg"),
    ("audio/opus", "opus"),
    ("audio/wav", "wav"),
    ("audio/x-flac", "flac"),
    ("audio/x-wav", "wav"),
    ("font/otf", "otf"),
    ("font/ttf", "ttf"),
    ("font/woff", "woff"),
    ("font/woff2", "woff2"),
    ("image/avif", "avif"),
    ("image/bmp", "bmp"),
    ("image/gif", "gif"),
    ("image/heic", "heic"),
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/svg+xml", "svg"),
    ("image/tiff", "tiff"),
    ("image/vnd.microsoft.icon", "ico"),
    ("image/webp", "webp"),
    ("image/x-icon", "ico"),
    ("text/calendar", "ics"),
    ("text/csv", "csv"),
    ("text/markdown", "md"),
    ("text/vtt", "vtt"),
    ("text/xml", "xml"),
    ("video/3gpp", "3gp"),
    ("video/mp2t", "ts"),
    ("video/mp4", "mp4"),
    ("video/mpeg", "mpg"),
    ("video/ogg", "ogv"),
    ("video/quicktime", "mov"),
    ("video/webm", "webm"),
    ("video/x-matroska", "mkv"),
    ("video/x-msvideo", "avi"),
];

/// The conventional extension of the media type `value`, if it is
/// specific.
pub(crate) fn type_extension(value: &str) -> Option<&'static str> {
    let essence = media_type(value)?;
    TYPE_EXTENSIONS
        .binary_search_by(|(known, _)| known.cmp(&essence.as_str()))
        .ok()
        .map(|i| TYPE_EXTENSIONS[i].1)
}

/// The file name a `Content-Disposition` value suggests: `filename*` in
/// UTF-8 if present, else `filename`.
fn disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    for param in params(value).skip(1) {
        let (name, raw) = param.split_once('=')?;
        let name = name.trim();
        let raw = raw.trim();
        if name.eq_ignore_ascii_case("filename*") {
            let (charset, rest) = raw.split_once('\'')?;
            let (_language, encoded) = rest.split_once('\'')?;
            if charset.eq_ignore_ascii_case("utf-8") {
                return percent_decode_str(encoded)
                    .decode_utf8()
                    .ok()
                    .map(|name| name.into_owned());
            }
        } else if name.eq_ignore_ascii_case("filename") {
            plain = Some(unquote(raw)?);
        }
    }
    plain
}

/// Splits a header value at semicolons outside quoted strings.
fn params(value: &str) -> impl Iterator<Item = &str> {
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    let mut cuts = Vec::new();
    for (i, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                cuts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    cuts.push(&value[start..]);
    cuts.into_iter()
}

/// A token, or a quoted string with its escapes removed.
fn unquote(raw: &str) -> Option<String> {
    let Some(inner) = raw.strip_prefix('"') else {
        return Some(raw.to_owned());
    };
    let inner = inner.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { chars.next()? } else { c });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn named(naming: Naming<'_>) -> Option<String> {
        naming.name().map(|n| n.as_str().to_owned())
    }

    const SIGNED: &str = "https://bucket.example/o/7f3a9?X-Amz-Signature=secret\
        &response-content-disposition=attachment%3B%20filename%3D%22Q3%20report.pdf%22";

    #[test]
    fn sources_are_taken_strongest_first() {
        let signed = url(SIGNED);
        let all = Naming {
            disposition: Some("header.zip"),
            provider: Some("provider.zip"),
            url: Some(&signed),
            media_type: Some("application/zip"),
        };
        assert_eq!(named(all).as_deref(), Some("header.zip"));
        let provider = Naming {
            disposition: None,
            ..all
        };
        assert_eq!(named(provider).as_deref(), Some("provider.zip"));
        let query = Naming {
            provider: None,
            ..provider
        };
        assert_eq!(named(query).as_deref(), Some("Q3 report.pdf"));
        let plain = url("https://bucket.example/o/archive.tar?X-Amz-Signature=secret");
        assert_eq!(
            named(Naming {
                url: Some(&plain),
                ..query
            })
            .as_deref(),
            Some("archive.tar")
        );
        // An invalid stronger source falls through to the next.
        let hostile = Naming {
            disposition: Some("../x"),
            provider: Some("a/b"),
            ..all
        };
        assert_eq!(named(hostile).as_deref(), Some("Q3 report.pdf"));
        assert_eq!(
            Naming::default().or_job(JobId::new(std::num::NonZeroU64::new(17).unwrap())),
            RelativePath::new("download-17").unwrap()
        );
    }

    #[test]
    fn signed_links_name_files_by_their_documented_parameters() {
        let name = |s: &str| named(Naming::of(&url(s)));
        assert_eq!(name(SIGNED).as_deref(), Some("Q3 report.pdf"));
        // Azure Blob SAS, with an RFC 8187 value and `+` for a space.
        assert_eq!(
            name(
                "https://acct.blob.example/c/blob?sv=2022-11-02&sig=secret\
                 &rscd=attachment%3B+filename%2A%3DUTF-8%27%27r%25C3%25A9sum%25C3%25A9.txt"
            )
            .as_deref(),
            Some("résumé.txt")
        );
        // No file name, another parameter, or two answers: the path names it.
        for other in [
            "https://h.example/o/blob.bin?response-content-disposition=attachment",
            "https://h.example/o/blob.bin?filename=private.zip",
            "https://h.example/o/blob.bin?rscd=attachment%3Bfilename%3Da.zip\
             &response-content-disposition=attachment%3Bfilename%3Db.zip",
        ] {
            assert_eq!(name(other).as_deref(), Some("blob.bin"), "{other}");
        }
    }

    #[test]
    fn hostile_names_never_choose_a_directory_or_an_invalid_file() {
        let long = format!("attachment; filename=\"{}.zip\"", "a".repeat(300));
        let disposition = |value: &str| {
            format!(
                "https://h.example/?rscd={}",
                percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
            )
        };
        for value in [
            "attachment; filename=\"../x\"",
            "attachment; filename=\"a/b\"",
            "attachment; filename=\"a\\\\b\"",
            "attachment; filename=..",
            "attachment; filename=CON.txt",
            "attachment; filename=\"aux\"",
            "attachment; filename=\".wayfetch\"",
            "attachment; filename=\"trailing. \"",
            "attachment; filename*=UTF-8''%2E%2E%2Fx",
            "attachment; filename*=UTF-8''a%0Ab.zip",
            // Not UTF-8, and a multi-byte character cut short.
            "attachment; filename*=UTF-8''%FF.zip",
            "attachment; filename*=UTF-8''%E2%82.zip",
            long.as_str(),
        ] {
            let link = url(&disposition(value));
            assert_eq!(named(Naming::of(&link)), None, "{value}");
            let header = Disposition::parse(value).filename;
            let naming = Naming {
                disposition: header.as_deref(),
                ..Naming::default()
            };
            assert_eq!(named(naming), None, "{value}");
        }
        // Bytes that are not UTF-8 in the query itself.
        assert_eq!(
            named(Naming::of(&url(
                "https://h.example/?rscd=attachment%3Bfilename%3D%FF.zip"
            ))),
            None
        );
        for path in [
            "https://h.example/%2E%2E",
            "https://h.example/a%2Fb.zip",
            "https://h.example/a%5Cb.zip",
            "https://h.example/%FF.zip",
            "https://h.example/NUL",
        ] {
            assert_eq!(named(Naming::of(&url(path))), None, "{path}");
        }
    }

    #[test]
    fn a_specific_type_gives_a_name_without_an_extension_one() {
        let link = url("https://h.example/files/download");
        let typed = |media_type| {
            named(Naming {
                media_type,
                ..Naming::of(&link)
            })
        };
        assert_eq!(typed(Some("image/jpeg")).as_deref(), Some("download.jpg"));
        assert_eq!(
            typed(Some("Application/PDF; charset=binary")).as_deref(),
            Some("download.pdf")
        );
        for generic in [
            None,
            Some("application/octet-stream"),
            Some("text/html; charset=utf-8"),
            Some("text/plain"),
            Some("application/x-msdownload"),
            Some(""),
        ] {
            assert_eq!(typed(generic).as_deref(), Some("download"), "{generic:?}");
        }
        // A name that has an extension keeps it, whatever the type.
        let named_zip = Naming {
            disposition: Some("data.bin"),
            media_type: Some("application/zip"),
            ..Naming::default()
        };
        assert_eq!(named(named_zip).as_deref(), Some("data.bin"));
        // The job's own name takes one too.
        let job = JobId::new(std::num::NonZeroU64::new(3).unwrap());
        let fallback = Naming {
            media_type: Some("video/mp4"),
            ..Naming::default()
        };
        assert_eq!(fallback.or_job(job).as_str(), "download-3.mp4");
        // One that would grow too long is left as it is.
        let longest = "a".repeat(255);
        let full = Naming {
            disposition: Some(&longest),
            media_type: Some("image/png"),
            ..Naming::default()
        };
        assert_eq!(named(full), Some(longest.clone()));
    }

    #[test]
    fn extensions_are_recognized_conservatively() {
        for name in ["a.zip", "archive.tar.gz", "v2.0", "x.WOFF2"] {
            assert!(has_extension(name), "{name}");
        }
        for name in [
            "download",
            ".profile",
            "v1.2 final",
            "a.",
            "a.toolongextension1",
        ] {
            assert!(!has_extension(name), "{name}");
        }
    }

    #[test]
    fn the_type_table_is_sorted_specific_and_conventional() {
        assert!(
            TYPE_EXTENSIONS.windows(2).all(|w| w[0].0 < w[1].0),
            "sorted and unique, for the binary search"
        );
        for (media_type, extension) in TYPE_EXTENSIONS {
            assert_eq!(self::media_type(media_type).as_deref(), Some(*media_type));
            assert!(media_type.contains('/'), "{media_type}");
            assert!(
                has_extension(&format!("x.{extension}")) && extension.to_lowercase() == *extension,
                "{extension}"
            );
        }
        for (media_type, extension) in [
            ("image/jpeg", "jpg"),
            ("audio/mpeg", "mp3"),
            ("video/quicktime", "mov"),
            ("application/vnd.ms-excel", "xls"),
            ("application/x-tar", "tar"),
        ] {
            assert_eq!(type_extension(media_type), Some(extension), "{media_type}");
        }
        for generic in [
            "application/octet-stream",
            "binary/octet-stream",
            "text/html",
            "application/xhtml+xml",
            "text/plain",
            "application/force-download",
            "application/x-msdownload",
            "application/x-sh",
            "text/javascript",
        ] {
            assert_eq!(type_extension(generic), None, "{generic}");
        }
    }

    #[test]
    fn disposition_names_prefer_the_utf8_form() {
        let name = |v| Disposition::parse(v).filename;
        assert_eq!(
            name("attachment; filename=\"plain.zip\"; filename*=UTF-8''%E2%82%AC%20rates.zip")
                .as_deref(),
            Some("€ rates.zip")
        );
        assert_eq!(
            name("attachment; filename=token.bin").as_deref(),
            Some("token.bin")
        );
        assert_eq!(
            name(r#"attachment; filename="semi;colon \"q\".txt""#).as_deref(),
            Some("semi;colon \"q\".txt")
        );
        assert_eq!(name("inline").as_deref(), None);
        assert_eq!(name("attachment; filename=\"unterminated").as_deref(), None);
        assert!(Disposition::parse("Attachment; filename=a").attachment);
        assert!(!Disposition::parse("inline; filename=a").attachment);
    }
}
