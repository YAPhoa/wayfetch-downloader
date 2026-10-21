//! Finding links in collected text and in fetched HTML, without the network.
//!
//! Links come once each, in order of first appearance and as written;
//! non-HTTP links are kept as invalid so the person sees why, and scripts
//! are never read.

use std::cell::Cell;
use std::collections::HashSet;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use linkify::{LinkFinder, LinkKind};
use scraper::node::Node;
use scraper::{Html, Selector};
use url::Url;
use wayfetch_protocol::command::InputUrl;

/// Most links read from one text or page. The rest are ignored, so a huge
/// page costs bounded work.
pub const MAX_LINKS_READ: usize = 20_000;

/// Elements whose attribute names a link worth collecting.
const LINKING: &[(&str, &str)] = &[
    ("a", "href"),
    ("area", "href"),
    ("iframe", "src"),
    ("frame", "src"),
    ("embed", "src"),
    ("video", "src"),
    ("audio", "src"),
    ("source", "src"),
];

/// One entry found in collected text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A link Wayfetch can check and download.
    Valid(InputUrl),
    /// Something written as a link that is not one Wayfetch can use.
    Invalid {
        /// The entry, without credentials.
        text: String,
        /// Why it cannot be used.
        problem: String,
    },
}

impl Entry {
    /// Checks one link as written.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        match InputUrl::parse(raw) {
            Ok(url) => Self::Valid(url),
            Err(e) => Self::Invalid {
                text: without_credentials(raw),
                problem: e.message.as_str().to_owned(),
            },
        }
    }

    /// What two entries are the same link by.
    fn key(&self) -> &str {
        match self {
            Self::Valid(url) => url.as_url().as_str(),
            Self::Invalid { text, .. } => text,
        }
    }
}

/// The links found in collected text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    /// Each link once, in order.
    pub entries: Vec<Entry>,
    /// Whether the text was read as HTML: its links are a page's content,
    /// not links the person typed.
    pub html: bool,
    /// Links found more than once, beyond the first.
    pub repeated: u32,
}

/// The links in `text`: HTML when it looks like markup or a page address
/// is given for it, plain text otherwise.
pub fn collected(text: &str, base: Option<&Url>) -> Collected {
    let html = base.is_some() || looks_like_html(text);
    let mut found = Found::default();
    if html {
        read_html(text, base, &mut found, |raw| Some(Entry::parse(raw)));
    } else {
        read_text(text, &mut found);
    }
    Collected {
        entries: found.entries,
        html,
        repeated: found.repeated,
    }
}

/// The `http(s)` links on a page fetched from `base`, in order, each once.
/// Links Wayfetch could not download are left out.
pub fn page_links(html: &str, base: &Url) -> Vec<Url> {
    let mut found = Found::default();
    read_html(html, Some(base), &mut found, |raw| {
        match Entry::parse(raw) {
            entry @ Entry::Valid(_) => Some(entry),
            Entry::Invalid { .. } => None,
        }
    });
    found
        .entries
        .into_iter()
        .filter_map(|e| match e {
            Entry::Valid(url) => Some(url.as_url().clone()),
            Entry::Invalid { .. } => None,
        })
        .collect()
}

/// Whether pasted text is markup: the HTML tokenizer reads a doctype, a
/// comment, or a tag written in it (`<html>` and `<body>` included),
/// however its tags are cased, spaced, or split across lines. The tree a
/// parser builds would not do, since it adds those wrappers to any text. A
/// link in angle brackets (`<https://…>`) or an address (`<a@b.example>`)
/// is not a tag: no HTML tag name has a `:`, `@`, or `.`.
fn looks_like_html(text: &str) -> bool {
    struct Markup(Cell<bool>);
    impl TokenSink for Markup {
        type Handle = ();
        fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
            let markup = match token {
                Token::DoctypeToken(_) | Token::CommentToken(_) => true,
                Token::TagToken(tag) => {
                    tag.name.starts_with(|c: char| c.is_ascii_alphabetic())
                        && tag
                            .name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-')
                }
                _ => false,
            };
            if markup {
                self.0.set(true);
            }
            TokenSinkResult::Continue
        }
    }

    if !text.contains('<') {
        return false;
    }
    let tokenizer = Tokenizer::new(Markup(Cell::new(false)), TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(text));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.0.get()
}

/// Entries found so far, each once.
#[derive(Default)]
struct Found {
    entries: Vec<Entry>,
    seen: HashSet<String>,
    read: usize,
    repeated: u32,
}

impl Found {
    /// Whether more links may be read.
    fn open(&self) -> bool {
        self.read < MAX_LINKS_READ
    }

    fn add(&mut self, entry: Entry) {
        self.read += 1;
        if self.seen.insert(entry.key().to_owned()) {
            self.entries.push(entry);
        } else {
            self.repeated += 1;
        }
    }
}

/// Undoes the escapes that hide links in JSON and BBCode, so they can be
/// found: `\/` and `/` for `/`, `&` for `&`, and `[url=…]`,
/// `[url]`, `[/url]`, `[img]`, `[/img]` tags, which become spaces.
fn unescaped(text: &str) -> String {
    let mut text = text
        .replace("\\/", "/")
        .replace("\\u002f", "/")
        .replace("\\u002F", "/")
        .replace("\\u0026", "&");
    for tag in ["[url=", "[URL="] {
        text = text.replace(tag, " ");
    }
    for tag in [
        "[url]", "[/url]", "[URL]", "[/URL]", "[img]", "[/img]", "[IMG]", "[/IMG]",
    ] {
        text = text.replace(tag, " ");
    }
    text
}

/// Reads the `scheme://` links in plain text.
fn read_text(text: &str, found: &mut Found) {
    let text = unescaped(text);
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    for link in finder.links(&text) {
        if !found.open() {
            return;
        }
        // BBCode's `[url=…]name` leaves the name after a closing bracket,
        // which only an IPv6 host may hold.
        let raw = link.as_str();
        let raw = match raw.split_once(']') {
            Some((link, _)) if !raw.contains("://[") => link,
            _ => raw,
        };
        found.add(Entry::parse(raw));
    }
}

/// Reads an HTML document: link targets read against `base` and the
/// document's own base, and links written in its text. `keep` decides
/// what an `http(s)` target found becomes.
fn read_html(
    html: &str,
    base: Option<&Url>,
    found: &mut Found,
    keep: impl Fn(&str) -> Option<Entry>,
) {
    let document = Html::parse_document(html);
    let base = document_base(&document, base);
    for node in document.root_element().descendants() {
        if !found.open() {
            return;
        }
        match node.value() {
            Node::Element(element) => {
                let name = element.name();
                let Some((_, attribute)) = LINKING.iter().find(|(tag, _)| *tag == name) else {
                    continue;
                };
                let Some(target) = element.attr(attribute).map(str::trim) else {
                    continue;
                };
                // An anchor within the page is not a link to fetch.
                if target.is_empty() || target.starts_with('#') {
                    continue;
                }
                let resolved = match &base {
                    Some(base) => base.join(target),
                    None => Url::parse(target),
                };
                let Ok(url) = resolved else { continue };
                if !matches!(url.scheme(), "http" | "https") {
                    continue;
                }
                if let Some(entry) = keep(url.as_str()) {
                    found.add(entry);
                }
            }
            Node::Text(text) => {
                let inside = node
                    .parent()
                    .and_then(|p| p.value().as_element().map(|e| e.name().to_owned()));
                if matches!(inside.as_deref(), Some("script" | "style" | "noscript")) {
                    continue;
                }
                let mut finder = LinkFinder::new();
                finder.kinds(&[LinkKind::Url]);
                for link in finder.links(&unescaped(text)) {
                    if !found.open() {
                        return;
                    }
                    let Ok(url) = Url::parse(link.as_str()) else {
                        continue;
                    };
                    if matches!(url.scheme(), "http" | "https")
                        && let Some(entry) = keep(link.as_str())
                    {
                        found.add(entry);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The address relative links in `document` are read against: its first
/// `<base href>`, itself read against `base`, else `base`.
fn document_base(document: &Html, base: Option<&Url>) -> Option<Url> {
    let selector = Selector::parse("base[href]").expect("a valid selector");
    let declared = document
        .select(&selector)
        .next()
        .and_then(|e| e.value().attr("href"))
        .map(str::trim);
    match (declared, base) {
        (Some(href), Some(base)) => base.join(href).ok().or_else(|| Some(base.clone())),
        (Some(href), None) => Url::parse(href).ok(),
        (None, base) => base.cloned(),
    }
    .filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// `text` without the credentials a link in it may carry, which are never
/// stored: from a parsed URL, and from text that only looks like one.
pub(crate) fn without_credentials(text: &str) -> String {
    if let Ok(mut url) = Url::parse(text) {
        if !url.username().is_empty() || url.password().is_some() {
            // Only a URL with a host has credentials, and those can be
            // cleared.
            let _ = url.set_username("");
            let _ = url.set_password(None);
            return url.into();
        }
        return text.to_owned();
    }
    if let Some(start) = text.find("://").map(|i| i + 3) {
        let rest = &text[start..];
        let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
        if let Some(at) = authority.rfind('@') {
            return format!("{}{}", &text[..start], &rest[at + 1..]);
        }
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(entries: &[Entry]) -> Vec<&str> {
        entries
            .iter()
            .filter_map(|e| match e {
                Entry::Valid(url) => Some(url.as_url().as_str()),
                Entry::Invalid { .. } => None,
            })
            .collect()
    }

    #[test]
    fn credentials_are_never_kept() {
        assert_eq!(
            without_credentials("https://user:pw@files.example/a?t=1"),
            "https://files.example/a?t=1"
        );
        assert_eq!(
            without_credentials("ftp://user:pw@files.example/a"),
            "ftp://files.example/a"
        );
        assert_eq!(without_credentials("htps//x y@z"), "htps//x y@z");
        assert_eq!(
            without_credentials("https://us er:p w@files .example/a"),
            "https://files .example/a"
        );
        assert_eq!(without_credentials("not a link"), "not a link");
    }

    #[test]
    fn text_links_come_once_in_order_without_their_punctuation() {
        let text = "Get it (https://a.example/one.zip), then https://a.example/two.zip.\n\
                    Again: https://a.example/one.zip and <https://b.example/x?a=1&a=2&b=%2F>\n\
                    [url=https://c.example/bb.rar]name[/url] [url]https://c.example/cc.rar[/url]\n\
                    {\"u\":\"https:\\/\\/d.example\\/json.bin\"} plain words, no links.";
        let found = collected(text, None);
        assert!(!found.html);
        assert_eq!(
            valid(&found.entries),
            [
                "https://a.example/one.zip",
                "https://a.example/two.zip",
                "https://b.example/x?a=1&a=2&b=%2F",
                "https://c.example/bb.rar",
                "https://c.example/cc.rar",
                "https://d.example/json.bin",
            ]
        );
        assert_eq!(found.repeated, 1);
    }

    #[test]
    fn signed_links_and_fragments_keep_their_meaning() {
        let link = "https://CDN.Example/Path/File.ZIP?b=2&a=1&a=0&sig=x%2Fy#key=Secret";
        let found = collected(link, None);
        assert_eq!(
            valid(&found.entries),
            ["https://cdn.example/Path/File.ZIP?b=2&a=1&a=0&sig=x%2Fy#key=Secret"]
        );
    }

    #[test]
    fn links_wayfetch_cannot_use_are_kept_as_invalid() {
        let found = collected(
            "ftp://user:pw@a.example/f https://u:p@b.example/g a.example/no-scheme",
            None,
        );
        assert_eq!(
            found.entries,
            [
                Entry::Invalid {
                    text: "ftp://a.example/f".into(),
                    problem: "only http and https URLs are supported".into()
                },
                Entry::Invalid {
                    text: "https://b.example/g".into(),
                    problem: "URLs must not contain credentials".into()
                },
            ]
        );
        assert!(collected("no links here at all", None).entries.is_empty());
    }

    #[test]
    fn html_links_are_read_against_the_page_and_its_base() {
        let html = r##"<html><head><base href="/files/"></head><body>
            <a href="a.zip">a</a> <a href='#top'>top</a> <a href=mailto:x@a.example>mail</a>
            <a href="javascript:go()">go</a> <img src="thumb.jpg">
            <a href="https://other.example/b.zip?x=1&amp;y=2">b</a>
            <video src="clip.mp4"></video> <a href=a.zip>again</a>
            Plain https://text.example/c.bin in text.
            <script>var u = "https://script.example/never.bin";</script>
        </body></html>"##;
        let base = Url::parse("https://site.example/release/page.html").unwrap();
        let found = collected(html, Some(&base));
        assert!(found.html);
        assert_eq!(
            valid(&found.entries),
            [
                "https://site.example/files/a.zip",
                "https://other.example/b.zip?x=1&y=2",
                "https://site.example/files/clip.mp4",
                "https://text.example/c.bin",
            ]
        );
        assert_eq!(found.repeated, 1);
    }

    #[test]
    fn pasted_html_without_an_address_keeps_only_absolute_links() {
        let found = collected(
            r#"<p><a href="rel.zip">r</a> <a href="https://a.example/abs.zip">a</a></p>"#,
            None,
        );
        assert!(found.html);
        assert_eq!(valid(&found.entries), ["https://a.example/abs.zip"]);
    }

    #[test]
    fn markup_is_html_however_its_tags_are_written() {
        for html in [
            "<a\nhref=\"http://127.0.0.1:8080/admin.bin\">Download</a>",
            "<A\tHREF='http://127.0.0.1:8080/admin.bin'>x</A>",
            "<video\r\nsrc=http://127.0.0.1:8080/admin.bin></video>",
            "<p>http://127.0.0.1:8080/admin.bin</p>",
            "<!doctype html><html><body>\nhttp://127.0.0.1:8080/admin.bin\n</body></html>\n",
            "<!doctype html>\nhttp://127.0.0.1:8080/admin.bin",
            "<html>http://127.0.0.1:8080/admin.bin</html>",
            "<HEAD></HEAD>http://127.0.0.1:8080/admin.bin",
            "<body\n>http://127.0.0.1:8080/admin.bin",
            "</body>http://127.0.0.1:8080/admin.bin",
            "<!-- saved -->http://127.0.0.1:8080/admin.bin",
        ] {
            let found = collected(html, None);
            assert!(found.html, "{html:?}");
            assert_eq!(valid(&found.entries), ["http://127.0.0.1:8080/admin.bin"]);
        }
        for text in [
            "<https://a.example/f.zip>",
            "mail <me@a.example> https://a.example/f.zip",
            "1 < 2 and 3 > 2: https://a.example/f.zip",
        ] {
            assert!(!collected(text, None).html, "{text:?}");
        }
    }

    #[test]
    fn page_links_leave_out_what_cannot_be_downloaded() {
        let base = Url::parse("https://site.example/d/").unwrap();
        let links = page_links(
            r#"<a href="ftp://x.example/f">f</a><a href="https://u:p@x.example/g">g</a><a href="h.zip">h</a>"#,
            &base,
        );
        assert_eq!(links, [Url::parse("https://site.example/d/h.zip").unwrap()]);
    }

    #[test]
    fn huge_inputs_read_a_bounded_number_of_links() {
        let text: String = (0..MAX_LINKS_READ + 50)
            .map(|i| format!("https://a.example/{i}.zip "))
            .collect();
        assert_eq!(collected(&text, None).entries.len(), MAX_LINKS_READ);
    }
}
