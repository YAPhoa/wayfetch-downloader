//! Link filters, compiled for matching links as they are collected.
//!
//! Conditions on the link's address and where it came from are judged as
//! it is collected, pasted or found on a page. A link that will be checked
//! and that a filter could still match stays off the list until its check,
//! when the filters as they are then judge it again, by the file name its
//! server gives and what else the check told (size, check result, an
//! account). A link that is never checked is judged by the name in its
//! address, and cannot meet conditions only a check tells.

use regex::{Regex, RegexBuilder};
use url::Url;
use wayfetch_protocol::filter::{
    CheckOutcome, Choice, FileKind, FileTypeCondition, FilterTrial, LinkFilters, LinkOrigin,
    LinkRoute, LinkRule, Membership, SizeCondition, TextCondition, TextOp,
};
use wayfetch_protocol::{ErrorCode, ProtocolError};

/// The most memory one compiled pattern may take, so a pattern typed in a
/// browser cannot make a huge matcher.
const COMPILED_LIMIT: usize = 1 << 18;

/// A text pattern: with `*` standing for any text unless `regex`, found
/// anywhere unless `whole`, and ignoring case.
fn pattern(text: &str, regex: bool, whole: bool) -> Result<Regex, regex::Error> {
    let body = if regex {
        text.to_owned()
    } else {
        text.split('*')
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(".*")
    };
    let body = if whole { format!("^(?:{body})$") } else { body };
    RegexBuilder::new(&body)
        .case_insensitive(true)
        .size_limit(COMPILED_LIMIT)
        .build()
}

/// A compiled text condition.
#[derive(Debug)]
struct Text {
    regex: Regex,
    negate: bool,
}

impl Text {
    fn compile(condition: &TextCondition) -> Result<Self, regex::Error> {
        let whole = matches!(condition.op, TextOp::Is | TextOp::IsNot);
        Ok(Self {
            regex: pattern(condition.pattern.as_str(), condition.regex, whole)?,
            negate: matches!(condition.op, TextOp::NotContains | TextOp::IsNot),
        })
    }

    fn holds(&self, text: &str) -> bool {
        self.regex.is_match(text) != self.negate
    }
}

/// The kind of file an extension, in lower case, names.
fn kind(extension: &str) -> Option<FileKind> {
    Some(match extension {
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "tbz2" | "xz" | "txz" | "zst"
        | "lz" | "lzma" | "cab" | "iso" | "img" | "dmg" => FileKind::Archive,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "ts"
        | "m2ts" | "3gp" | "ogv" => FileKind::Video,
        "mp3" | "flac" | "wav" | "aac" | "m4a" | "ogg" | "oga" | "opus" | "wma" | "aiff"
        | "alac" => FileKind::Audio,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "svg" | "heic"
        | "avif" => FileKind::Image,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
        | "txt" | "rtf" | "epub" | "mobi" | "azw3" | "csv" | "md" => FileKind::Document,
        // Parts of split archives: `.r00`, `.001`.
        _ if extension.len() == 3
            && extension.bytes().skip(1).all(|b| b.is_ascii_digit())
            && (extension.starts_with('r') || extension.as_bytes()[0].is_ascii_digit()) =>
        {
            FileKind::Archive
        }
        _ => return None,
    })
}

/// A compiled file type condition.
#[derive(Debug)]
struct Types {
    kinds: Vec<FileKind>,
    extensions: Vec<Regex>,
    negate: bool,
}

impl Types {
    fn compile(condition: &FileTypeCondition) -> Result<Self, regex::Error> {
        Ok(Self {
            kinds: condition.kinds.clone(),
            extensions: condition
                .extensions
                .iter()
                .map(|e| pattern(e.as_str().trim_start_matches('.'), false, true))
                .collect::<Result<_, _>>()?,
            negate: condition.op == Membership::IsNot,
        })
    }

    fn holds(&self, name: Option<&str>) -> bool {
        let extension = name
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, e)| e.to_ascii_lowercase())
            .filter(|e| !e.is_empty());
        let hit = extension.is_some_and(|e| {
            kind(&e).is_some_and(|k| self.kinds.contains(&k))
                || self.extensions.iter().any(|r| r.is_match(&e))
        });
        hit != self.negate
    }
}

/// What a link's check told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Answer<'a> {
    /// The file name the server gave, if any.
    pub(crate) name: Option<&'a str>,
    pub(crate) size: Option<u64>,
    pub(crate) check: CheckOutcome,
    pub(crate) route: LinkRoute,
}

/// A compiled filter.
#[derive(Debug)]
struct Rule {
    file_name: Option<Text>,
    file_type: Option<Types>,
    link: Option<Text>,
    found_on: Option<Text>,
    added_from: Option<Choice<LinkOrigin>>,
    size: Option<SizeCondition>,
    check: Option<Choice<CheckOutcome>>,
    route: Option<Choice<LinkRoute>>,
}

/// A link being collected, as filters see it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Link<'a> {
    pub(crate) url: &'a Url,
    pub(crate) origin: LinkOrigin,
    /// The page it was found on, or the address given with pasted markup.
    pub(crate) found_on: Option<&'a Url>,
    /// Whether a media service recognized it.
    pub(crate) media: bool,
    /// Whether it will be checked.
    pub(crate) checked: bool,
}

/// What filters decide for a link being collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// List it.
    Keep,
    /// Leave it out.
    Drop,
    /// Hold it back until its check, which decides.
    Wait,
}

impl Rule {
    /// Whether the conditions on the link's address and where it came
    /// from hold, which are known as it is collected.
    fn holds_for_address(&self, link: &Link<'_>) -> bool {
        let found_on = link.found_on.map_or("", Url::as_str);
        self.link.as_ref().is_none_or(|t| t.holds(link.url.as_str()))
            && self.found_on.as_ref().is_none_or(|t| t.holds(found_on))
            && self
                .added_from
                .as_ref()
                .is_none_or(|c| c.holds(&link.origin))
            // A media service is known as the link is collected.
            && (!link.media || self.route.as_ref().is_none_or(|c| c.holds(&LinkRoute::Media)))
    }

    /// Whether the conditions on the file's name hold for `name`.
    fn holds_for_name(&self, name: Option<&str>) -> bool {
        self.file_name
            .as_ref()
            .is_none_or(|t| t.holds(name.unwrap_or("")))
            && self.file_type.as_ref().is_none_or(|t| t.holds(name))
    }

    fn names(&self) -> bool {
        self.file_name.is_some() || self.file_type.is_some()
    }

    /// Whether the rule has conditions only a check tells for `link`.
    fn needs_answer(&self, link: &Link<'_>) -> bool {
        self.size.is_some() || self.check.is_some() || (self.route.is_some() && !link.media)
    }

    /// Whether the conditions only a check tells hold for `answer`. A
    /// size the server did not say meets none.
    fn holds_for_answer(&self, answer: &Answer<'_>) -> bool {
        let size = self.size.is_none_or(|c| {
            answer.size.is_some_and(|s| {
                let within = s >= c.min && c.max.is_none_or(|max| s <= max);
                within == (c.op == Membership::Is)
            })
        });
        size && self.check.as_ref().is_none_or(|c| c.holds(&answer.check))
            && self.route.as_ref().is_none_or(|c| c.holds(&answer.route))
    }
}

/// The filters that apply, compiled.
#[derive(Debug, Default)]
pub(crate) struct Filters(Vec<Rule>);

impl Filters {
    /// Compiles every filter, refusing one whose pattern is not a valid
    /// regular expression, and keeps those that apply.
    pub(crate) fn compile(filters: &LinkFilters) -> Result<Self, ProtocolError> {
        let mut rules = Vec::new();
        for rule in &filters.filters {
            let refuse = |what: &str, error: regex::Error| {
                let why = match error {
                    regex::Error::CompiledTooBig(_) => "is too complex",
                    _ => "is not a valid regular expression",
                };
                ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    &format!(
                        "filter \"{}\": the {what} pattern {why}",
                        rule.name.as_str()
                    ),
                )
            };
            let text = |condition: &Option<TextCondition>, what: &str| {
                condition
                    .as_ref()
                    .map(Text::compile)
                    .transpose()
                    .map_err(|e| refuse(what, e))
            };
            let compiled = Rule {
                file_name: text(&rule.file_name, "file name")?,
                link: text(&rule.link, "link")?,
                found_on: text(&rule.found_on, "found on")?,
                file_type: rule
                    .file_type
                    .as_ref()
                    .map(Types::compile)
                    .transpose()
                    .map_err(|e| refuse("extension", e))?,
                added_from: rule.added_from.clone(),
                size: rule.size,
                check: rule.check.clone(),
                route: rule.route.clone(),
            };
            if filters.enabled && rule.enabled {
                rules.push(compiled);
            }
        }
        Ok(Self(rules))
    }

    /// Decides whether `link` is listed as it is collected: dropped if a
    /// filter matches it outright, held for its check if one could still
    /// match then. A link that will be checked is judged by the file name
    /// its server gives, so conditions on the name wait for the check
    /// too; one never checked cannot meet conditions only a check tells.
    pub(crate) fn judge(&self, link: &Link<'_>) -> Verdict {
        let name = crate::download::url_file_name(link.url);
        let name = name.as_ref().map(|n| n.as_str());
        let mut wait = false;
        for rule in &self.0 {
            if !rule.holds_for_address(link) {
                continue;
            }
            if link.checked {
                if !rule.names() && !rule.needs_answer(link) {
                    return Verdict::Drop;
                }
                wait = true;
            } else if !rule.needs_answer(link) && rule.holds_for_name(name) {
                return Verdict::Drop;
            }
        }
        if wait { Verdict::Wait } else { Verdict::Keep }
    }

    /// Whether a filter leaves out `link`, held back for its check, now
    /// that `answer` tells what it found: by the name the server gave, or
    /// else the one in its address.
    pub(crate) fn leaves_out(&self, link: &Link<'_>, answer: &Answer<'_>) -> bool {
        let from_url = crate::download::url_file_name(link.url);
        let name = answer
            .name
            .or_else(|| from_url.as_ref().map(|n| n.as_str()));
        self.0.iter().any(|rule| {
            rule.holds_for_address(link)
                && rule.holds_for_name(name)
                && rule.holds_for_answer(answer)
        })
    }
}

/// What `rule` would do with `url` pasted as text, enabled or not, taking
/// the file name from the address as its server would most often give it.
pub(crate) fn trial(
    mut rule: LinkRule,
    url: &Url,
    media: bool,
) -> Result<FilterTrial, ProtocolError> {
    rule.enabled = true;
    let filters = Filters::compile(&LinkFilters {
        enabled: true,
        filters: vec![rule],
    })?;
    let link = Link {
        url,
        origin: LinkOrigin::Text,
        found_on: None,
        media,
        checked: true,
    };
    let name = crate::download::url_file_name(url);
    let name = name.as_ref().map(|n| n.as_str());
    let Some(rule) = filters.0.first() else {
        return Ok(FilterTrial::Kept);
    };
    Ok(
        if !rule.holds_for_address(&link) || !rule.holds_for_name(name) {
            FilterTrial::Kept
        } else if rule.needs_answer(&link) {
            FilterTrial::CheckDecides
        } else {
            FilterTrial::LeftOut
        },
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn filters(rules: serde_json::Value) -> Filters {
        let filters: LinkFilters =
            serde_json::from_value(json!({ "enabled": true, "filters": rules })).unwrap();
        Filters::compile(&filters).unwrap()
    }

    fn rule(conditions: serde_json::Value) -> serde_json::Value {
        let mut rule = json!({ "name": "r", "enabled": true });
        rule.as_object_mut()
            .unwrap()
            .extend(conditions.as_object().unwrap().clone());
        rule
    }

    /// Pasted, and not checked, so judged by its address alone.
    fn pasted(url: &Url) -> Link<'_> {
        Link {
            url,
            origin: LinkOrigin::Text,
            found_on: None,
            media: false,
            checked: false,
        }
    }

    /// Pasted, and checked.
    fn checked(url: &Url) -> Link<'_> {
        Link {
            checked: true,
            ..pasted(url)
        }
    }

    fn online<'a>(name: Option<&'a str>, size: Option<u64>) -> Answer<'a> {
        Answer {
            name,
            size,
            check: CheckOutcome::Online,
            route: LinkRoute::Direct,
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn text(op: &str, pattern: &str) -> serde_json::Value {
        json!({ "op": op, "pattern": pattern })
    }

    #[test]
    fn text_conditions_use_wildcards_ignore_case_and_negate() {
        let link = url("https://CDN.example.com/files/Movie.Sample.MKV?sig=1");
        let judge = |c| filters(json!([rule(c)])).judge(&pasted(&link));
        assert_eq!(
            judge(json!({ "file_name": text("contains", "sample") })),
            Verdict::Drop
        );
        assert_eq!(
            judge(json!({ "file_name": text("is", "*.mkv") })),
            Verdict::Drop
        );
        assert_eq!(
            judge(json!({ "file_name": text("is", "sample") })),
            Verdict::Keep
        );
        assert_eq!(
            judge(json!({ "link": text("contains", "*.example.com/files") })),
            Verdict::Drop
        );
        assert_eq!(
            judge(json!({ "link": text("not_contains", "example.com") })),
            Verdict::Keep
        );
        // Without `regex`, regular expression syntax is plain text.
        assert_eq!(
            judge(json!({ "file_name": text("contains", "mo.ie") })),
            Verdict::Keep
        );
        assert_eq!(
            judge(json!({ "file_name": { "op": "is", "pattern": "mo.ie\\..*", "regex": true } })),
            Verdict::Drop
        );
    }

    #[test]
    fn every_condition_of_a_filter_must_hold_and_any_filter_drops() {
        let link = url("https://a.example/x.zip");
        let both = filters(json!([rule(json!({
            "file_type": { "op": "is", "kinds": ["archive"] },
            "link": text("contains", "b.example"),
        }))]));
        assert_eq!(both.judge(&pasted(&link)), Verdict::Keep);
        let either = filters(json!([
            rule(json!({ "link": text("contains", "b.example") })),
            rule(json!({ "file_type": { "op": "is", "kinds": ["archive"] } })),
        ]));
        assert_eq!(either.judge(&pasted(&link)), Verdict::Drop);
    }

    #[test]
    fn file_types_go_by_kind_or_named_extension() {
        let types = |c: serde_json::Value, s: &str| {
            filters(json!([rule(json!({ "file_type": c }))])).judge(&pasted(&url(s)))
                == Verdict::Drop
        };
        let archives = json!({ "op": "is", "kinds": ["archive"] });
        assert!(types(archives.clone(), "https://a.example/x.part1.RAR"));
        assert!(types(archives.clone(), "https://a.example/x.r07"));
        assert!(types(archives.clone(), "https://a.example/x.7z.001"));
        assert!(!types(archives.clone(), "https://a.example/x.mkv"));
        assert!(!types(archives, "https://a.example/download"));
        let named = json!({ "op": "is", "extensions": ["nfo", ".sf*"] });
        assert!(types(named.clone(), "https://a.example/x.NFO"));
        assert!(types(named.clone(), "https://a.example/x.sfv"));
        assert!(!types(named, "https://a.example/x.txt"));
        // A link without a name is no image, so "is not" drops it.
        let not_images = json!({ "op": "is_not", "kinds": ["image"] });
        assert!(types(not_images.clone(), "https://a.example/"));
        assert!(!types(not_images, "https://a.example/a.png"));
    }

    #[test]
    fn found_on_and_origin_tell_how_a_link_came() {
        let link = url("https://cdn.example/x.zip");
        let page = url("https://blog.example/post");
        let found = Link {
            origin: LinkOrigin::AnalyzedPage,
            found_on: Some(&page),
            ..pasted(&link)
        };
        let set = filters(json!([rule(json!({
            "found_on": text("contains", "blog.example"),
            "added_from": { "op": "is", "values": ["analyzed_page", "web_page"] },
        }))]));
        assert_eq!(set.judge(&found), Verdict::Drop);
        assert_eq!(set.judge(&pasted(&link)), Verdict::Keep);
    }

    #[test]
    fn conditions_a_check_tells_wait_for_it_or_cannot_match() {
        let link = url("https://a.example/x.zip");
        let small = filters(json!([rule(json!({
            "file_type": { "op": "is", "kinds": ["archive"] },
            "size": { "op": "is", "max": 1000 },
        }))]));
        assert_eq!(small.judge(&checked(&link)), Verdict::Wait);
        assert!(small.leaves_out(&checked(&link), &online(None, Some(10))));
        assert!(!small.leaves_out(&checked(&link), &online(None, Some(5000))));
        assert!(!small.leaves_out(&checked(&link), &online(None, None)));
        // A link that will not be checked is kept.
        assert_eq!(small.judge(&pasted(&link)), Verdict::Keep);
        // Conditions on the address that fail decide without waiting.
        let address = filters(json!([rule(json!({
            "link": text("contains", "b.example"),
            "size": { "op": "is", "max": 1000 },
        }))]));
        assert_eq!(address.judge(&checked(&link)), Verdict::Keep);
        // Ones on the address alone decide at once.
        let host = filters(json!([rule(
            json!({ "link": text("contains", "a.example") })
        )]));
        assert_eq!(host.judge(&checked(&link)), Verdict::Drop);
    }

    #[test]
    fn a_checked_link_is_judged_by_the_name_its_server_gives() {
        let link = url("https://a.example/download/123");
        let samples = filters(json!([rule(
            json!({ "file_name": text("contains", "sample") })
        )]));
        assert_eq!(samples.judge(&checked(&link)), Verdict::Wait);
        assert!(samples.leaves_out(&checked(&link), &online(Some("sample.zip"), None)));
        assert!(!samples.leaves_out(&checked(&link), &online(Some("film.zip"), None)));
        // A negative type waits for the name too, instead of dropping a
        // link whose address names no file.
        let not_video = filters(json!([rule(
            json!({ "file_type": { "op": "is_not", "kinds": ["video"] } })
        )]));
        assert_eq!(not_video.judge(&checked(&link)), Verdict::Wait);
        assert!(!not_video.leaves_out(&checked(&link), &online(Some("film.mkv"), None)));
        assert!(not_video.leaves_out(&checked(&link), &online(Some("notes.txt"), None)));
        // Without a name from the server, the address's is used.
        let named = url("https://a.example/files/sample.zip");
        assert!(samples.leaves_out(&checked(&named), &online(None, None)));
    }

    #[test]
    fn a_media_link_is_known_to_go_through_its_service() {
        let link = url("https://video.example/watch?v=1");
        let media = Link {
            media: true,
            ..checked(&link)
        };
        let via = |values: serde_json::Value| {
            filters(json!([rule(
                json!({ "route": { "op": "is", "values": values } })
            )]))
            .judge(&media)
        };
        assert_eq!(via(json!(["media"])), Verdict::Drop);
        assert_eq!(via(json!(["account"])), Verdict::Keep);
        let accounts = filters(json!([rule(
            json!({ "route": { "op": "is", "values": ["account"] } })
        )]));
        assert_eq!(accounts.judge(&checked(&link)), Verdict::Wait);
        let answer = Answer {
            route: LinkRoute::Account,
            ..online(None, None)
        };
        assert!(accounts.leaves_out(&checked(&link), &answer));
    }

    #[test]
    fn a_trial_tells_what_one_filter_would_do_even_when_disabled() {
        let rule = |c: serde_json::Value| -> LinkRule {
            let mut rule = rule(c);
            rule["enabled"] = json!(false);
            serde_json::from_value(rule).unwrap()
        };
        let link = url("https://a.example/x.zip");
        let try_rule = |c| trial(rule(c), &link, false).unwrap();
        assert_eq!(
            try_rule(json!({ "link": text("contains", "a.example") })),
            FilterTrial::LeftOut
        );
        assert_eq!(
            try_rule(json!({ "link": text("contains", "b.example") })),
            FilterTrial::Kept
        );
        assert_eq!(
            try_rule(json!({ "size": { "op": "is", "max": 10 } })),
            FilterTrial::CheckDecides
        );
        assert_eq!(
            try_rule(json!({ "file_name": text("contains", "x") })),
            FilterTrial::LeftOut
        );
    }

    #[test]
    fn invalid_patterns_are_refused_even_in_disabled_filters_which_never_apply() {
        let invalid: LinkFilters = serde_json::from_value(json!({
            "enabled": true,
            "filters": [{ "name": "Bad", "enabled": false,
                "link": { "op": "contains", "pattern": "(", "regex": true } }],
        }))
        .unwrap();
        let error = Filters::compile(&invalid).unwrap_err();
        assert!(
            error
                .message
                .as_str()
                .contains("\"Bad\": the link pattern is not a valid")
        );
        let off = filters(json!([{ "name": "Off", "enabled": false,
            "link": text("contains", "a") }]));
        assert_eq!(
            off.judge(&pasted(&url("https://a.example/"))),
            Verdict::Keep
        );
        let disabled: LinkFilters = serde_json::from_value(json!({
            "enabled": false,
            "filters": [rule(json!({ "link": text("contains", "a") }))],
        }))
        .unwrap();
        let disabled = Filters::compile(&disabled).unwrap();
        assert_eq!(
            disabled.judge(&pasted(&url("https://a.example/"))),
            Verdict::Keep
        );
    }
}
