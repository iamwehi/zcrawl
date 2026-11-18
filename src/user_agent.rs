//! Parsing BIP 14 user agents into an implementation and a version.
//!
//! A user agent is a list of `/name:version/` components, each optionally
//! followed by a parenthesised comment, for example
//! `/MagicBean:6.0.0(bitcore)/` or `/Zebra:2.5.0/`. The first component whose
//! name is a known Zcash node (`Zebra`, `Zakura`, `MagicBean`) decides the
//! implementation. A well-formed user agent with no known name is `other`.
//! Anything that does not follow the format, or a known name whose version is
//! not `x.y.z[-pre][+build]`, is `unknown`.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use crate::params::MAX_USER_AGENT_LENGTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Implementation {
    Zebra,
    Zakura,
    /// zcashd, which announces itself as `MagicBean`.
    Zcashd,
    /// A well-formed user agent naming some other software.
    Other,
    /// A user agent that could not be parsed.
    Unknown,
}

impl Implementation {
    /// The value stored in `observations.implementation`.
    pub fn as_str(self) -> &'static str {
        match self {
            Implementation::Zebra => "zebra",
            Implementation::Zakura => "zakura",
            Implementation::Zcashd => "zcashd",
            Implementation::Other => "other",
            Implementation::Unknown => "unknown",
        }
    }

    fn from_component_name(name: &str) -> Option<Self> {
        match name {
            "Zebra" => Some(Implementation::Zebra),
            "Zakura" => Some(Implementation::Zakura),
            "MagicBean" => Some(Implementation::Zcashd),
            _ => None,
        }
    }
}

impl fmt::Display for Implementation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A semantic version. Build metadata is dropped; pre-release versions sort
/// before the release they precede, as in SemVer 2.0.
///
/// One exception: Zebra and Zakura turn `git describe`'s `-dirty` into
/// `-modified`, so a node built from the exact `v7.1.0` tag with local changes
/// announces `7.1.0-modified`. That is the 7.1.0 release, not a pre-release of
/// it, so a lone `modified` pre-release tag is kept for display but ignored
/// when comparing.
#[derive(Debug, Clone)]
pub struct NodeVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<PreId>,
    /// The version was `x.y.z-modified`.
    pub modified: bool,
}

impl PartialEq for NodeVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for NodeVersion {}

impl std::hash::Hash for NodeVersion {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.major, self.minor, self.patch, &self.pre).hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PreId {
    Numeric(u64),
    Alpha(String),
}

impl Ord for PreId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PreId::Numeric(a), PreId::Numeric(b)) => a.cmp(b),
            (PreId::Numeric(_), PreId::Alpha(_)) => Ordering::Less,
            (PreId::Alpha(_), PreId::Numeric(_)) => Ordering::Greater,
            (PreId::Alpha(a), PreId::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NodeVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for NodeVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for NodeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if self.modified {
            f.write_str("-modified")?;
        }
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                PreId::Numeric(n) => write!(f, "{n}")?,
                PreId::Alpha(s) => f.write_str(s)?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionParseError;

impl fmt::Display for VersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a version of the form x.y.z or x.y.z-pre")
    }
}

impl std::error::Error for VersionParseError {}

impl FromStr for NodeVersion {
    type Err = VersionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Build metadata does not take part in ordering.
        let s = match s.split_once('+') {
            Some((v, build)) if is_ident_list(build) => v,
            Some(_) => return Err(VersionParseError),
            None => s,
        };
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(VersionParseError);
        };
        let pre = match pre {
            None => Vec::new(),
            Some(pre) if is_ident_list(pre) => pre
                .split('.')
                .map(|id| match parse_numeric(id) {
                    Some(n) => PreId::Numeric(n),
                    None => PreId::Alpha(id.to_owned()),
                })
                .collect(),
            Some(_) => return Err(VersionParseError),
        };
        let modified = matches!(pre.as_slice(), [PreId::Alpha(m)] if m == "modified");
        Ok(NodeVersion {
            major: parse_numeric(major).ok_or(VersionParseError)?,
            minor: parse_numeric(minor).ok_or(VersionParseError)?,
            patch: parse_numeric(patch).ok_or(VersionParseError)?,
            pre: if modified { Vec::new() } else { pre },
            modified,
        })
    }
}

/// A non-empty decimal number without a leading zero.
fn parse_numeric(s: &str) -> Option<u64> {
    let valid =
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    if valid { s.parse().ok() } else { None }
}

/// Dot-separated, non-empty identifiers of `[0-9A-Za-z-]`.
fn is_ident_list(s: &str) -> bool {
    s.split('.')
        .all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUserAgent {
    pub implementation: Implementation,
    /// For `zebra`, `zakura` and `zcashd`, always set. For `other`, set when
    /// the component's version happens to be a valid version. For `unknown`,
    /// never set.
    pub version: Option<NodeVersion>,
}

impl ParsedUserAgent {
    fn unknown() -> Self {
        ParsedUserAgent {
            implementation: Implementation::Unknown,
            version: None,
        }
    }
}

/// Parse a raw user agent as sent on the wire.
pub fn parse_user_agent(raw: &[u8]) -> ParsedUserAgent {
    let Ok(s) = std::str::from_utf8(raw) else {
        return ParsedUserAgent::unknown();
    };
    let Some(components) = split_components(s) else {
        return ParsedUserAgent::unknown();
    };

    let mut first_other: Option<&str> = None;
    for (name, version) in &components {
        if let Some(implementation) = Implementation::from_component_name(name) {
            return match version.parse() {
                Ok(v) => ParsedUserAgent {
                    implementation,
                    version: Some(v),
                },
                Err(_) => ParsedUserAgent::unknown(),
            };
        }
        first_other.get_or_insert(version);
    }
    ParsedUserAgent {
        implementation: Implementation::Other,
        version: first_other.and_then(|v| v.parse().ok()),
    }
}

/// Split `/name:version(comment)/name:version/` into `(name, version)` pairs.
/// Returns `None` unless the whole string follows that format.
fn split_components(s: &str) -> Option<Vec<(&str, &str)>> {
    if s.len() > MAX_USER_AGENT_LENGTH || s.len() < 2 || !s.starts_with('/') || !s.ends_with('/') {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let mut components = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1)?,
            '/' if depth == 0 => {
                components.push(parse_component(&inner[start..i])?);
                start = i + 1;
            }
            c if c.is_control() => return None,
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    components.push(parse_component(&inner[start..])?);
    Some(components)
}

/// Parse `name:version` or `name:version(comment)`.
fn parse_component(c: &str) -> Option<(&str, &str)> {
    let (name, rest) = c.split_once(':')?;
    let version = match rest.find('(') {
        Some(open) => {
            // The comment must close at the very end of the component.
            if !rest.ends_with(')') {
                return None;
            }
            &rest[..open]
        }
        None => rest,
    };
    let name_ok = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, ' ' | '.' | '-' | '_'));
    let version_ok = !version.is_empty() && !version.contains([')', ':']);
    (name_ok && version_ok).then_some((name, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> (Implementation, Option<String>) {
        let p = parse_user_agent(s.as_bytes());
        (p.implementation, p.version.map(|v| v.to_string()))
    }

    fn known(i: Implementation, v: &str) -> (Implementation, Option<String>) {
        (i, Some(v.to_owned()))
    }

    const UNKNOWN: (Implementation, Option<String>) = (Implementation::Unknown, None);

    #[test]
    fn zebra() {
        assert_eq!(
            parse("/Zebra:2.5.0/"),
            known(Implementation::Zebra, "2.5.0")
        );
        assert_eq!(
            parse("/Zebra:10.21.300/"),
            known(Implementation::Zebra, "10.21.300")
        );
        assert_eq!(
            parse("/Zebra:2.0.0-rc.0/"),
            known(Implementation::Zebra, "2.0.0-rc.0")
        );
        assert_eq!(
            parse("/Zebra:1.0.0-beta.10/"),
            known(Implementation::Zebra, "1.0.0-beta.10")
        );
    }

    #[test]
    fn zakura() {
        assert_eq!(
            parse("/Zakura:1.6.1/"),
            known(Implementation::Zakura, "1.6.1")
        );
        assert_eq!(
            parse("/Zakura:2.0.0-alpha.3/"),
            known(Implementation::Zakura, "2.0.0-alpha.3")
        );
    }

    #[test]
    fn magicbean_is_zcashd() {
        assert_eq!(
            parse("/MagicBean:6.0.0/"),
            known(Implementation::Zcashd, "6.0.0")
        );
        assert_eq!(
            parse("/MagicBean:5.1.0-rc1/"),
            known(Implementation::Zcashd, "5.1.0-rc1")
        );
    }

    #[test]
    fn comment_fields() {
        assert_eq!(
            parse("/MagicBean:6.0.0(bitcore)/"),
            known(Implementation::Zcashd, "6.0.0")
        );
        assert_eq!(
            parse("/Zebra:2.5.0(linux; x86_64; docker)/"),
            known(Implementation::Zebra, "2.5.0")
        );
        assert_eq!(
            parse("/Zakura:1.6.1()/"),
            known(Implementation::Zakura, "1.6.1")
        );
        // A '/' inside a comment does not split components.
        assert_eq!(
            parse("/Zebra:2.5.0(see a/b)/"),
            known(Implementation::Zebra, "2.5.0")
        );
    }

    #[test]
    fn several_components_first_known_wins() {
        assert_eq!(
            parse("/Zebra:2.5.0/zcrawl:0.1.0/"),
            known(Implementation::Zebra, "2.5.0")
        );
        assert_eq!(
            parse("/proxy:1.0.0/Zakura:1.6.1/"),
            known(Implementation::Zakura, "1.6.1")
        );
        assert_eq!(
            parse("/Zakura:1.6.1/Zebra:2.5.0/"),
            known(Implementation::Zakura, "1.6.1")
        );
    }

    #[test]
    fn build_metadata_is_dropped() {
        assert_eq!(
            parse("/Zebra:2.5.0+3.g1234abc/"),
            known(Implementation::Zebra, "2.5.0")
        );
    }

    #[test]
    fn other_software() {
        assert_eq!(
            parse("/zcrawl:0.1.0/"),
            (Implementation::Other, Some("0.1.0".into()))
        );
        assert_eq!(
            parse("/Satoshi:0.9.99/"),
            (Implementation::Other, Some("0.9.99".into()))
        );
        assert_eq!(
            parse("/Bitcoin ABC:0.22(EB32.0)/"),
            (Implementation::Other, None)
        );
        // Names are matched exactly.
        assert_eq!(
            parse("/zebra:2.5.0/"),
            (Implementation::Other, Some("2.5.0".into()))
        );
    }

    #[test]
    fn garbage_is_unknown() {
        for s in [
            "",
            "/",
            "//",
            "Zebra:2.5.0",
            "/Zebra:2.5.0",
            "Zebra:2.5.0/",
            "/Zebra/",
            "/Zebra:/",
            "/:2.5.0/",
            "/Zebra:2.5/",
            "/Zebra:2.5.0.1/",
            "/Zebra:v2.5.0/",
            "/Zebra:02.5.0/",
            "/Zebra:2.5.0-/",
            "/Zebra:2.5.0-rc..1/",
            "/Zebra:99999999999999999999999.0.0/",
            "/Zebra:2.5.0(unclosed/",
            "/Zebra:2.5.0)(/",
            "/Zebra:2.5.0(a)b/",
            "/Zebra:2.5.0//",
            "/Zebra:2.5.0/garbage/",
            "/Ze\u{0}bra:2.5.0/",
            "/Zebra:2.5.0\n/",
            "hello world",
        ] {
            assert_eq!(parse(s), UNKNOWN, "{s:?}");
        }
        assert_eq!(
            parse_user_agent(&[b'/', 0xff, 0xfe, b'/']),
            ParsedUserAgent::unknown()
        );
        let long = format!("/Zebra:2.5.0({})/", "x".repeat(300));
        assert_eq!(parse(&long), UNKNOWN);
    }

    #[test]
    fn version_ordering() {
        let v = |s: &str| s.parse::<NodeVersion>().unwrap();
        assert!(v("2.0.0-rc.0") < v("2.0.0"));
        assert!(v("2.0.0-alpha") < v("2.0.0-alpha.1"));
        assert!(v("2.0.0-alpha.1") < v("2.0.0-alpha.beta"));
        assert!(v("2.0.0-beta.2") < v("2.0.0-beta.11"));
        assert!(v("2.0.0-rc.1") < v("2.0.0"));
        assert!(v("2.0.0") < v("2.0.1"));
        assert!(v("2.0.1") < v("2.1.0-rc.1"));
        assert!(v("2.9.0") < v("2.10.0"));
        assert!(v("1.99.99") < v("2.0.0"));
        assert_eq!(v("2.5.0+abc"), v("2.5.0"));
    }

    #[test]
    fn modified_build_counts_as_the_release() {
        let v = |s: &str| s.parse::<NodeVersion>().unwrap();
        assert_eq!(v("7.1.0-modified"), v("7.1.0"));
        assert!(v("7.1.0-modified") >= v("7.1.0"));
        assert!(v("7.1.0-modified") > v("7.1.0-rc.1"));
        assert!(v("7.1.0-modified") < v("7.1.1"));
        // Only a lone `modified` tag is special.
        assert!(v("7.1.0-modified.1") < v("7.1.0"));
        // The suffix is kept for display and storage.
        assert_eq!(
            parse("/Zebra:7.1.0-modified/"),
            known(Implementation::Zebra, "7.1.0-modified")
        );
    }
}
