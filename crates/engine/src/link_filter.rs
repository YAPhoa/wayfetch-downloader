//! Link filters, compiled for matching links as they are collected.
//!
//! Conditions on the link itself are judged as it is collected, pasted or
//! found on a page. Those on what only a check tells (size, check result,
//! an account) travel with a link that will be checked, which stays off
//! the list until its check decides; a link that is never checked cannot
//! meet them, so it is kept.

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
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

/// A filter's conditions on what only a check tells, kept as JSON with a
/// link that waits for its check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pending {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<SizeCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    check: Option<Choice<CheckOutcome>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    route: Option<Choice<LinkRoute>>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.size.is_none() && self.check.is_none() && self.route.is_none()
    }

    /// Whether `answer` meets every condition. A size the server did not
    /// say meets none.
    fn holds(&self, answer: &Answer) -> bool {
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

/// What a link's check told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Answer {
    pub(crate) size: Option<u64>,
    pub(crate) check: CheckOutcome,
    pub(crate) route: LinkRoute,
}

/// Whether any of the conditions a link waited with holds for its check.
pub(crate) fn any_holds(pending: &[Pending], answer: &Answer) -> bool {
    pending.iter().any(|p| p.holds(answer))
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// List it.
    Keep,
    /// Leave it out.
    Drop,
    /// Hold it back until its check tells whether any of these hold.
    Wait(Vec<Pending>),
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

    /// Decides whether `link` is listed: dropped if a filter matches it
    /// outright, held for its check if one could still match then.
    pub(crate) fn judge(&self, link: &Link<'_>) -> Verdict {
        if self.0.is_empty() {
            return Verdict::Keep;
        }
        let name = crate::download::url_file_name(link.url);
        let name = name.as_ref().map(|n| n.as_str());
        let found_on = link.found_on.map_or("", Url::as_str);
        let mut waits = Vec::new();
        for rule in &self.0 {
            let now = rule
                .file_name
                .as_ref()
                .is_none_or(|t| t.holds(name.unwrap_or("")))
                && rule.file_type.as_ref().is_none_or(|t| t.holds(name))
                && rule.link.as_ref().is_none_or(|t| t.holds(link.url.as_str()))
                && rule.found_on.as_ref().is_none_or(|t| t.holds(found_on))
                && rule
                    .added_from
                    .as_ref()
                    .is_none_or(|c| c.holds(&link.origin))
                // A media service is known as the link is collected.
                && (!link.media || rule.route.as_ref().is_none_or(|c| c.holds(&LinkRoute::Media)));
            if !now {
                continue;
            }
            let later = Pending {
                size: rule.size,
                check: rule.check.clone(),
                route: rule.route.clone().filter(|_| !link.media),
            };
            if later.is_empty() {
                return Verdict::Drop;
            }
            if link.checked {
                waits.push(later);
            }
        }
        if waits.is_empty() {
            Verdict::Keep
        } else {
            Verdict::Wait(waits)
        }
    }
}

/// What `rule` would do with `url` pasted as text, enabled or not.
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
    Ok(
        match filters.judge(&Link {
            url,
            origin: LinkOrigin::Text,
            found_on: None,
            media,
            checked: true,
        }) {
            Verdict::Drop => FilterTrial::LeftOut,
            Verdict::Keep => FilterTrial::Kept,
            Verdict::Wait(_) => FilterTrial::CheckDecides,
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

    fn pasted(url: &Url) -> Link<'_> {
        Link {
            url,
            origin: LinkOrigin::Text,
            found_on: None,
            media: false,
            checked: true,
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
        let Verdict::Wait(pending) = small.judge(&pasted(&link)) else {
            panic!("a size waits for the check");
        };
        let answer = |size| Answer {
            size,
            check: CheckOutcome::Online,
            route: LinkRoute::Direct,
        };
        assert!(any_holds(&pending, &answer(Some(10))));
        assert!(!any_holds(&pending, &answer(Some(5000))));
        assert!(!any_holds(&pending, &answer(None)));
        // A link that will not be checked is kept.
        let unchecked = Link {
            checked: false,
            ..pasted(&link)
        };
        assert_eq!(small.judge(&unchecked), Verdict::Keep);
        // Conditions on the link that fail decide without waiting.
        let other = url("https://a.example/x.mkv");
        assert_eq!(small.judge(&pasted(&other)), Verdict::Keep);
        // The stored form round-trips.
        let stored = serde_json::to_string(&pending).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<Pending>>(&stored).unwrap(),
            pending
        );
    }

    #[test]
    fn a_media_link_is_known_to_go_through_its_service() {
        let link = url("https://video.example/watch?v=1");
        let media = Link {
            media: true,
            ..pasted(&link)
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
        let Verdict::Wait(pending) = accounts.judge(&pasted(&link)) else {
            panic!("an account is told by the check");
        };
        let answer = Answer {
            size: None,
            check: CheckOutcome::Online,
            route: LinkRoute::Account,
        };
        assert!(any_holds(&pending, &answer));
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
