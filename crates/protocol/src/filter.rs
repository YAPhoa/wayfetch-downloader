//! Link filters: named rules that keep matching links out of the collected
//! list. A link a filter matches is dropped as it is collected, whether it
//! was pasted or found on a page. Conditions on what only a check tells
//! (size, check result, an account) hold the link back until its check.

use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, ProtocolError};

/// The most filters the settings keep.
pub const MAX_LINK_FILTERS: usize = 100;
/// The longest filter name, in bytes.
pub const MAX_RULE_NAME_BYTES: usize = 80;
/// The longest pattern, in bytes.
pub const MAX_PATTERN_BYTES: usize = 500;
/// The most extensions a file type condition lists.
pub const MAX_EXTENSIONS: usize = 32;

/// Trimmed text of 1 to `max` bytes without control characters.
fn bounded_text(text: &str, what: &str, max: usize) -> Result<String, ProtocolError> {
    let text = text.trim();
    let invalid = |m: &str| ProtocolError::new(ErrorCode::InvalidRequest, m);
    if text.is_empty() {
        return Err(invalid(&format!("a {what} must not be empty")));
    }
    if text.len() > max {
        return Err(ProtocolError::new(
            ErrorCode::LimitExceeded,
            &format!("a {what} is at most {max} bytes"),
        ));
    }
    if text.chars().any(char::is_control) {
        return Err(invalid(&format!(
            "a {what} must not contain control characters"
        )));
    }
    Ok(text.to_owned())
}

/// A filter's name: 1 to [`MAX_RULE_NAME_BYTES`] bytes, trimmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, type = "string"))]
pub struct RuleName(String);

impl RuleName {
    /// Validates a name.
    pub fn new(text: &str) -> Result<Self, ProtocolError> {
        bounded_text(text, "filter name", MAX_RULE_NAME_BYTES).map(Self)
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RuleName {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<RuleName> for String {
    fn from(value: RuleName) -> Self {
        value.0
    }
}

/// What a text condition looks for: 1 to [`MAX_PATTERN_BYTES`] bytes,
/// trimmed. `*` stands for any text, unless it is a regular expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, type = "string"))]
pub struct Pattern(String);

impl Pattern {
    /// Validates a pattern.
    pub fn new(text: &str) -> Result<Self, ProtocolError> {
        bounded_text(text, "pattern", MAX_PATTERN_BYTES).map(Self)
    }

    /// The pattern.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Pattern {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<Pattern> for String {
    fn from(value: Pattern) -> Self {
        value.0
    }
}

/// How a text condition compares. Comparisons ignore case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum TextOp {
    /// The pattern is found anywhere in the text.
    Contains,
    /// The pattern is found nowhere in it.
    NotContains,
    /// The pattern matches the whole text.
    Is,
    /// The pattern does not match the whole text.
    IsNot,
}

/// Whether a condition holds when the value is among those listed, or
/// when it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Membership {
    /// It is one of them.
    Is,
    /// It is none of them.
    IsNot,
}

/// A condition on text: a file name or a link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TextCondition {
    /// How it compares.
    pub op: TextOp,
    /// What it looks for.
    pub pattern: Pattern,
    /// Whether the pattern is a regular expression rather than text with
    /// `*` wildcards.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
    pub regex: bool,
}

/// A condition on a file's size, which only its check tells. A file whose
/// server did not say never matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SizeCondition {
    /// Is: the size is within the range; is not: outside it.
    pub op: Membership,
    /// The smallest size in the range, in bytes.
    #[serde(default)]
    pub min: u64,
    /// The largest size in the range, in bytes; absent for no upper end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max: Option<u64>,
}

/// A kind of file, told by its name's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum FileKind {
    /// Archives and disc images.
    Archive,
    /// Video.
    Video,
    /// Audio.
    Audio,
    /// Images.
    Image,
    /// Documents and books.
    Document,
}

/// A condition on a file's type, by its name's extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FileTypeCondition {
    /// Whether the type is, or is not, one of those listed.
    pub op: Membership,
    /// Kinds of file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<FileKind>>", optional))]
    pub kinds: Vec<FileKind>,
    /// Further extensions, such as `nfo` or `r*`, without the dot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<Pattern>>", optional))]
    pub extensions: Vec<Pattern>,
}

/// How a link was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum LinkOrigin {
    /// Typed or pasted as text.
    Text,
    /// In a pasted web page's markup.
    WebPage,
    /// Found when a page was analyzed.
    AnalyzedPage,
}

/// What a link's check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum CheckOutcome {
    /// A file, or a video, that can be downloaded.
    Online,
    /// Gone.
    Offline,
    /// The check could not tell.
    Unknown,
    /// A web page rather than a file.
    Page,
}

/// What a link would be downloaded through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum LinkRoute {
    /// Its own server.
    Direct,
    /// An enabled provider account, as its check found.
    Account,
    /// A media service, for a link one recognizes such as a video page.
    Media,
}

/// A condition that a value is, or is not, one of those listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Choice<T> {
    /// Is or is not.
    pub op: Membership,
    /// The values, at least one.
    pub values: Vec<T>,
}

impl<T: PartialEq> Choice<T> {
    /// Whether `value` meets the condition.
    pub fn holds(&self, value: &T) -> bool {
        self.values.contains(value) == (self.op == Membership::Is)
    }
}

/// A filter: a link it matches is not collected. Every condition it has
/// must hold; a filter has at least one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LinkRule {
    /// Its name, shown where it left links out.
    pub name: RuleName,
    /// Whether it applies.
    pub enabled: bool,
    /// The file's name: from the link until its check, which may tell
    /// another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub file_name: Option<TextCondition>,
    /// The file's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub size: Option<SizeCondition>,
    /// The file's type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub file_type: Option<FileTypeCondition>,
    /// The whole link, query included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub link: Option<TextCondition>,
    /// The page the link was found on, or the address given with pasted
    /// markup; empty for a link pasted as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub found_on: Option<TextCondition>,
    /// How the link was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub added_from: Option<Choice<LinkOrigin>>,
    /// What its check found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub check: Option<Choice<CheckOutcome>>,
    /// What it would be downloaded through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub route: Option<Choice<LinkRoute>>,
}

impl LinkRule {
    /// Refuses a filter without conditions, which would match every link,
    /// or with a condition that can never be told apart.
    fn validate(&self) -> Result<(), ProtocolError> {
        let name = self.name.as_str();
        let invalid = |m: &str| {
            Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                &format!("filter \"{name}\": {m}"),
            ))
        };
        let any = self.file_name.is_some()
            || self.size.is_some()
            || self.file_type.is_some()
            || self.link.is_some()
            || self.found_on.is_some()
            || self.added_from.is_some()
            || self.check.is_some()
            || self.route.is_some();
        if !any {
            return invalid("choose at least one condition");
        }
        if let Some(size) = &self.size
            && size.max.is_some_and(|max| max < size.min)
        {
            return invalid("the largest size is smaller than the smallest");
        }
        if let Some(types) = &self.file_type {
            if types.kinds.is_empty() && types.extensions.is_empty() {
                return invalid("choose a file type or name an extension");
            }
            if types.extensions.len() > MAX_EXTENSIONS {
                return invalid(&format!("name at most {MAX_EXTENSIONS} extensions"));
            }
        }
        let empty = self
            .added_from
            .as_ref()
            .is_some_and(|c| c.values.is_empty())
            || self.check.as_ref().is_some_and(|c| c.values.is_empty())
            || self.route.as_ref().is_some_and(|c| c.values.is_empty());
        if empty {
            return invalid("choose at least one value for each condition");
        }
        Ok(())
    }
}

/// What a filter would do with a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum FilterTrial {
    /// Leave it out.
    LeftOut,
    /// Keep it.
    Kept,
    /// Hold it until its check tells its size, check result, or account.
    CheckDecides,
}

/// The link filters: whether they apply, and the filters in order. At
/// most [`MAX_LINK_FILTERS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedLinkFilters", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LinkFilters {
    /// Whether filters apply at all.
    pub enabled: bool,
    /// The filters.
    pub filters: Vec<LinkRule>,
}

impl Default for LinkFilters {
    fn default() -> Self {
        Self {
            enabled: true,
            filters: Vec::new(),
        }
    }
}

/// [`LinkFilters`] before its rules are checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedLinkFilters {
    enabled: bool,
    filters: Vec<LinkRule>,
}

impl TryFrom<UncheckedLinkFilters> for LinkFilters {
    type Error = ProtocolError;

    fn try_from(value: UncheckedLinkFilters) -> Result<Self, Self::Error> {
        if value.filters.len() > MAX_LINK_FILTERS {
            return Err(ProtocolError::new(
                ErrorCode::LimitExceeded,
                &format!("keep at most {MAX_LINK_FILTERS} link filters"),
            ));
        }
        for rule in &value.filters {
            rule.validate()?;
        }
        Ok(Self {
            enabled: value.enabled,
            filters: value.filters,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn decode(value: serde_json::Value) -> Result<LinkFilters, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    #[test]
    fn filters_round_trip_without_absent_conditions() {
        let value = json!({
            "enabled": true,
            "filters": [{
                "name": " Samples ",
                "enabled": true,
                "file_name": { "op": "contains", "pattern": "sample" },
                "size": { "op": "is", "min": 0, "max": 1_000_000 },
                "check": { "op": "is", "values": ["offline"] },
            }],
        });
        let filters = decode(value.clone()).unwrap();
        assert_eq!(filters.filters[0].name.as_str(), "Samples");
        let mut expected = value;
        expected["filters"][0]["name"] = json!("Samples");
        assert_eq!(serde_json::to_value(&filters).unwrap(), expected);
    }

    #[test]
    fn filters_that_cannot_work_are_refused() {
        let rule = |extra: serde_json::Value| {
            let mut rule = json!({ "name": "x", "enabled": true });
            rule.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            decode(json!({ "enabled": true, "filters": [rule] }))
        };
        assert!(
            rule(json!({}))
                .unwrap_err()
                .contains("at least one condition")
        );
        assert!(
            rule(json!({ "size": { "op": "is", "min": 10, "max": 5 } }))
                .unwrap_err()
                .contains("smaller")
        );
        assert!(
            rule(json!({ "file_type": { "op": "is" } }))
                .unwrap_err()
                .contains("file type")
        );
        assert!(
            rule(json!({ "route": { "op": "is", "values": [] } }))
                .unwrap_err()
                .contains("at least one value")
        );
        assert!(
            rule(json!({ "link": { "op": "is", "pattern": "  " } }))
                .unwrap_err()
                .contains("empty")
        );
        let many: Vec<_> = (0..=MAX_LINK_FILTERS)
            .map(|i| json!({ "name": format!("f{i}"), "enabled": true, "check": { "op": "is", "values": ["offline"] } }))
            .collect();
        assert!(
            decode(json!({ "enabled": true, "filters": many }))
                .unwrap_err()
                .contains("at most")
        );
    }

    #[test]
    fn a_choice_holds_for_listed_values_or_for_the_rest() {
        let is = Choice {
            op: Membership::Is,
            values: vec![LinkRoute::Account],
        };
        assert!(is.holds(&LinkRoute::Account) && !is.holds(&LinkRoute::Direct));
        let not = Choice {
            op: Membership::IsNot,
            ..is
        };
        assert!(!not.holds(&LinkRoute::Account) && not.holds(&LinkRoute::Direct));
    }
}
