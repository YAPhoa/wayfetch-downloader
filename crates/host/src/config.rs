//! The operator's configuration file (architecture brief section 11.3).
//!
//! A host reads one TOML file, named with `--config`, at startup; nothing
//! looks for one anywhere else, and changes need a restart. It holds what
//! every host shares (the state directory, permitted roots, network
//! exceptions, request deadlines, and the policy for preferences users
//! change at run time); a host-specific section, such as `wayfetchd`'s
//! `[server]`, is typed by that host. Users' own changes are saved in the
//! engine's database, never written back here.
//!
//! Precedence: an option given on the command line replaces the file's
//! value, and a repeatable option (`--root`, `--allow-network`,
//! `--ca-cert`) adds to the file's list. Relative paths in the file are
//! resolved against the file's directory; relative paths on the command
//! line against the working directory.
//!
//! The file holds no secrets. Errors name the line, column, and key, but
//! never quote the file or a value from it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer};
use wayfetch_engine::protocol::query::SpeedLimit;
use wayfetch_engine::{
    EngineConfig, IntegerPolicy, Policy, Preference, SpeedLimitPolicy, Timeouts,
};
use wayfetch_net::ipnet::IpNet;

/// The schema version this release reads.
pub const SCHEMA_VERSION: u32 = 1;

/// Largest configuration file accepted.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Longest request deadline accepted, in seconds.
const MAX_TIMEOUT_SECONDS: u64 = 3600;

/// A configuration file as written, with `S` its host's own `[server]`
/// section.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "S: Deserialize<'de>"))]
pub struct ConfigFile<S> {
    /// Must be [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Where state and downloads live.
    #[serde(default)]
    pub storage: Storage,
    /// Exceptions to the production network policy.
    #[serde(default)]
    pub network: Network,
    /// Request deadlines.
    #[serde(default)]
    pub timeouts: TimeoutSeconds,
    /// Policy for download preferences.
    #[serde(default)]
    pub downloads: Downloads,
    /// The host's own section.
    #[serde(default = "Option::default")]
    pub server: Option<S>,
}

/// `[storage]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    /// The engine's state directory.
    pub state_dir: Option<PathBuf>,
    /// Where downloads are saved until users choose another default.
    pub download_root: Option<PathBuf>,
    /// Other folders users may choose as the default destination.
    #[serde(default)]
    pub roots: Vec<PathBuf>,
}

/// `[network]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Non-public networks downloads may also reach.
    #[serde(default)]
    pub allow_networks: Vec<IpNet>,
    /// Further certificate authorities to trust, PEM or DER.
    #[serde(default)]
    pub ca_certs: Vec<PathBuf>,
}

/// `[timeouts]`, in whole seconds; the built-in deadline where absent.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeoutSeconds {
    /// Establishing a connection.
    pub connect_seconds: Option<u64>,
    /// Receiving response headers, connecting included.
    pub response_seconds: Option<u64>,
    /// Waiting for the next body data.
    pub transfer_idle_seconds: Option<u64>,
}

/// `[downloads]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Downloads {
    /// `[downloads.max_active]`: most downloads at once.
    pub max_active: Option<PolicyFile>,
    /// `[downloads.speed_limit]`: the total download speed limit.
    pub speed_limit: Option<SpeedLimitFile>,
}

/// The operator's policy for the total download speed limit, as written,
/// in bytes per second. As for [`PolicyFile`], only `locked = true` fixes
/// the value. A `max` is a ceiling Unlimited cannot bypass.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeedLimitFile {
    /// The limit until a user saves another: bytes per second, or
    /// `"unlimited"`.
    pub default: Option<RateFile>,
    /// Smallest limit users may choose.
    pub min: Option<u64>,
    /// Largest limit users may choose; it also forbids Unlimited.
    pub max: Option<u64>,
    /// Whether users may not change it.
    #[serde(default)]
    pub locked: bool,
}

/// A speed limit as written: a whole number of bytes per second, or the
/// word `"unlimited"`. No number means Unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateFile(pub SpeedLimit);

impl<'de> Deserialize<'de> for RateFile {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = RateFile;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a whole number of bytes per second, or \"unlimited\"")
            }

            fn visit_u64<E: serde::de::Error>(self, rate: u64) -> Result<RateFile, E> {
                Ok(RateFile(SpeedLimit::BytesPerSecond(rate)))
            }

            fn visit_i64<E: serde::de::Error>(self, rate: i64) -> Result<RateFile, E> {
                u64::try_from(rate)
                    .map(|rate| RateFile(SpeedLimit::BytesPerSecond(rate)))
                    .map_err(|_| E::custom("a speed limit cannot be negative"))
            }

            fn visit_str<E: serde::de::Error>(self, word: &str) -> Result<RateFile, E> {
                if word == "unlimited" {
                    Ok(RateFile(SpeedLimit::Unlimited))
                } else {
                    Err(E::custom(
                        "expected a whole number of bytes per second, or `unlimited`",
                    ))
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl SpeedLimitFile {
    fn policy(&self) -> Result<SpeedLimitPolicy, String> {
        if self.locked && self.default.is_none() {
            return Err(format!(
                "{}: `locked = true` needs a `default` to lock",
                SpeedLimitPolicy::NAME
            ));
        }
        Ok(SpeedLimitPolicy {
            default: self.default.map(|d| d.0),
            min: self.min.unwrap_or(SpeedLimitPolicy::MIN),
            max: self.max,
            locked: self.locked,
        })
    }
}

/// The operator's policy for one preference, as written. A default alone
/// never fixes the value: only `locked = true` does, and it needs a
/// default to fix.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    /// The value until a user saves another.
    pub default: Option<u64>,
    /// Smallest value users may choose.
    pub min: Option<u64>,
    /// Largest value users may choose.
    pub max: Option<u64>,
    /// Whether users may not change it.
    #[serde(default)]
    pub locked: bool,
}

impl PolicyFile {
    fn policy(&self, preference: Preference) -> Result<IntegerPolicy, String> {
        if self.locked && self.default.is_none() {
            return Err(format!(
                "{}: `locked = true` needs a `default` to lock",
                preference.name
            ));
        }
        Ok(IntegerPolicy {
            default: self.default,
            min: self.min.unwrap_or(preference.min),
            max: self.max.unwrap_or(preference.max),
            locked: self.locked,
        })
    }
}

/// The `[server]` section of a host without a listener: refused, since
/// silently ignoring it could leave an operator believing a setting
/// applies.
#[derive(Debug)]
pub enum NoServer {}

impl<'de> Deserialize<'de> for NoServer {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(D::Error::custom(
            "the [server] section configures wayfetchd; this program has no listener",
        ))
    }
}

/// A configuration file read from disk.
#[derive(Debug)]
pub struct Loaded<S> {
    /// The file, with every path in the shared sections made absolute.
    pub file: ConfigFile<S>,
    /// The file's directory, against which its relative paths resolve.
    pub dir: PathBuf,
}

impl<S> Loaded<S> {
    /// `path` from the file, made absolute against the file's directory.
    pub fn path(&self, path: &Path) -> PathBuf {
        self.dir.join(path)
    }
}

/// Reads and checks the configuration file at `path`. A missing,
/// unreadable, oversized, or invalid file is an error: a host never
/// starts with settings other than those requested.
pub fn load<S: DeserializeOwned>(path: &Path) -> Result<Loaded<S>, String> {
    use std::io::Read;

    let path = crate::absolute(path)?;
    let fail = |e: &dyn std::fmt::Display| format!("configuration file {}: {e}", path.display());
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| fail(&e))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(fail(&"file too large"));
    }
    let text = String::from_utf8(bytes).map_err(|_| fail(&"not UTF-8 text"))?;
    let mut file: ConfigFile<S> = toml::from_str(&text).map_err(|e| fail(&describe(&text, &e)))?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(fail(&format!(
            "schema_version {} is not supported; this release reads {SCHEMA_VERSION}",
            file.schema_version
        )));
    }
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| fail(&"has no parent directory"))?;
    let resolve = |p: &mut PathBuf| *p = dir.join(&*p);
    let storage = &mut file.storage;
    storage.state_dir.iter_mut().for_each(resolve);
    storage.download_root.iter_mut().for_each(resolve);
    storage.roots.iter_mut().for_each(resolve);
    file.network.ca_certs.iter_mut().for_each(resolve);
    Ok(Loaded { file, dir })
}

/// Where a TOML error is and what it says, without quoting the file or
/// any value from it.
fn describe(text: &str, error: &toml::de::Error) -> String {
    let mut out = String::new();
    if let Some(span) = error.span() {
        let before = &text[..span.start.min(text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        let _ = write!(out, "line {line}, column {column}: ");
    }
    out.push_str(&redact(error.message().trim()));
    out
}

/// `message` with every double-quoted part, which serde uses for values,
/// replaced. Keys and expectations are quoted with backticks and stay.
fn redact(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut parts = message.split('"');
    out.push_str(parts.next().unwrap_or_default());
    while let Some(_value) = parts.next() {
        out.push_str("\"…\"");
        match parts.next() {
            Some(rest) => out.push_str(rest),
            None => break,
        }
    }
    out
}

/// Applies the file's request deadlines over the built-in ones.
pub(crate) fn timeouts(file: &TimeoutSeconds) -> Result<Timeouts, String> {
    let seconds = |name: &str, value: Option<u64>, built_in: Duration| match value {
        None => Ok(built_in),
        Some(s @ 1..=MAX_TIMEOUT_SECONDS) => Ok(Duration::from_secs(s)),
        Some(s) => Err(format!(
            "timeouts.{name} is {s}; it must be between 1 and {MAX_TIMEOUT_SECONDS}"
        )),
    };
    let timeouts = Timeouts {
        connect: seconds(
            "connect_seconds",
            file.connect_seconds,
            Timeouts::DEFAULT.connect,
        )?,
        response: seconds(
            "response_seconds",
            file.response_seconds,
            Timeouts::DEFAULT.response,
        )?,
        idle_read: seconds(
            "transfer_idle_seconds",
            file.transfer_idle_seconds,
            Timeouts::DEFAULT.idle_read,
        )?,
    };
    if timeouts.response < timeouts.connect {
        return Err(
            "timeouts.response_seconds must not be shorter than timeouts.connect_seconds, \
             since waiting for a response includes connecting"
                .to_owned(),
        );
    }
    Ok(timeouts)
}

/// Applies the file's preference policy over the built-in one.
pub(crate) fn policy(file: &Downloads) -> Result<Policy, String> {
    let mut policy = Policy::DEFAULT;
    if let Some(max_active) = &file.max_active {
        policy.max_active_downloads = max_active.policy(Preference::MAX_ACTIVE_DOWNLOADS)?;
    }
    if let Some(speed_limit) = &file.speed_limit {
        policy.download_speed_limit = speed_limit.policy()?;
    }
    Ok(policy)
}

/// Applies everything the file says about the engine to `config`, then
/// checks the result.
pub(crate) fn apply<S>(file: &ConfigFile<S>, config: &mut EngineConfig) -> Result<(), String> {
    config.timeouts = timeouts(&file.timeouts)?;
    config.policy = policy(&file.downloads)?;
    config.validate().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("wayfetch.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn parse(text: &str) -> Result<Loaded<NoServer>, String> {
        let tmp = tempfile::tempdir().unwrap();
        load(&write(tmp.path(), text))
    }

    #[test]
    fn relative_paths_resolve_against_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            r#"
schema_version = 1

[storage]
state_dir = "state"
download_root = "/srv/downloads"
roots = ["media", "../shared"]

[network]
allow_networks = ["192.168.1.0/24"]
ca_certs = ["ca.pem"]
"#,
        );
        let loaded = load::<NoServer>(&path).unwrap();
        let storage = &loaded.file.storage;
        let dir = crate::absolute(tmp.path()).unwrap();
        assert_eq!(storage.state_dir, Some(dir.join("state")));
        assert_eq!(storage.download_root, Some("/srv/downloads".into()));
        assert_eq!(storage.roots, [dir.join("media"), dir.join("../shared")]);
        assert_eq!(loaded.file.network.ca_certs, [dir.join("ca.pem")]);
        assert_eq!(
            loaded.file.network.allow_networks,
            ["192.168.1.0/24".parse::<IpNet>().unwrap()]
        );
        assert_eq!(loaded.path(Path::new("tokens")), dir.join("tokens"));
    }

    #[test]
    fn unknown_keys_and_bad_values_are_refused_without_quoting_them() {
        let unknown = parse("schema_version = 1\n[storage]\nstate_dirr = \"x\"\n").unwrap_err();
        assert!(unknown.contains("line 3, column 1"), "{unknown}");
        assert!(unknown.contains("state_dirr"), "{unknown}");

        let secret = "hunter2-not-a-network";
        let bad = parse(&format!(
            "schema_version = 1\n[network]\nallow_networks = [\"{secret}\"]\n"
        ))
        .unwrap_err();
        assert!(bad.contains("line 3"), "{bad}");
        assert!(!bad.contains(secret), "{bad}");

        let section =
            parse("schema_version = 1\n[server]\nlisten = \"127.0.0.1:1\"\n").unwrap_err();
        assert!(section.contains("configures wayfetchd"), "{section}");

        assert!(parse("[storage]\n").unwrap_err().contains("schema_version"));
        assert!(
            parse("schema_version = 2\n")
                .unwrap_err()
                .contains("schema_version 2 is not supported")
        );
        assert!(parse("schema_version = 1\n[downloads.max_active]\ndefault = -1\n").is_err());
        assert!(parse("schema_version = 1\nnot toml").is_err());
    }

    #[test]
    fn an_explicitly_named_file_must_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = load::<NoServer>(&tmp.path().join("missing.toml")).unwrap_err();
        assert!(missing.contains("missing.toml"), "{missing}");
        let huge = write(tmp.path(), &"#".repeat(MAX_FILE_BYTES as usize + 1));
        assert!(load::<NoServer>(&huge).unwrap_err().contains("too large"));
    }

    #[test]
    fn redaction_keeps_keys_and_drops_values() {
        assert_eq!(
            redact(r#"invalid type: string "s3cret", expected u64"#),
            r#"invalid type: string "…", expected u64"#
        );
        assert_eq!(
            redact("unknown field `x`, expected `a`"),
            "unknown field `x`, expected `a`"
        );
        assert_eq!(redact(r#"odd "quote"#), r#"odd "…""#);
    }

    #[test]
    fn the_speed_limit_policy_is_explicit_about_unlimited() {
        let applied = |text: &str| {
            let loaded = parse(&format!(
                "schema_version = 1\n[downloads.speed_limit]\n{text}"
            ))?;
            let mut config = EngineConfig::new("/state");
            apply(&loaded.file, &mut config)?;
            Ok::<_, String>(config.policy.download_speed_limit)
        };
        assert_eq!(applied(""), Ok(SpeedLimitPolicy::UNRESTRICTED));
        assert_eq!(
            applied("default = 2097152\nmax = 8388608"),
            Ok(SpeedLimitPolicy {
                default: Some(SpeedLimit::BytesPerSecond(2_097_152)),
                min: SpeedLimitPolicy::MIN,
                max: Some(8_388_608),
                locked: false,
            })
        );
        assert_eq!(
            applied("default = \"unlimited\"\nlocked = true").map(|p| (p.default, p.locked)),
            Ok((Some(SpeedLimit::Unlimited), true))
        );
        // A maximum forbids Unlimited, whether the default says so or is
        // left to the built-in one.
        for text in ["max = 8388608", "default = \"unlimited\"\nmax = 8388608"] {
            let refused = applied(text).unwrap_err();
            assert!(refused.contains("forbids Unlimited"), "{refused}");
        }
        // Zero is not a way to write Unlimited, and fractions, negatives,
        // other words, and units are refused.
        assert!(
            applied("default = 0")
                .unwrap_err()
                .contains("downloads.speed_limit")
        );
        for text in [
            "default = 1.5",
            "default = -1",
            "default = \"none\"",
            "default = \"2 MiB/s\"",
            "min = 1.5",
            "default = 18446744073709551616",
        ] {
            assert!(applied(text).is_err(), "{text}");
        }
        assert!(
            applied("locked = true")
                .unwrap_err()
                .contains("needs a `default`")
        );
        assert!(
            applied("max = 1099511627777")
                .unwrap_err()
                .contains("must lie within")
        );
        assert!(applied("bytes = 1").unwrap_err().contains("bytes"));
    }

    #[test]
    fn deadlines_and_policies_apply_over_the_built_in_ones() {
        let loaded = parse(
            r#"
schema_version = 1
[timeouts]
connect_seconds = 10
transfer_idle_seconds = 120
[downloads.max_active]
default = 6
max = 8
"#,
        )
        .unwrap();
        let mut config = EngineConfig::new("/state");
        apply(&loaded.file, &mut config).unwrap();
        assert_eq!(config.timeouts.connect, Duration::from_secs(10));
        assert_eq!(config.timeouts.response, Timeouts::DEFAULT.response);
        assert_eq!(config.timeouts.idle_read, Duration::from_secs(120));
        assert_eq!(
            config.policy.max_active_downloads,
            IntegerPolicy {
                default: Some(6),
                min: 1,
                max: 8,
                locked: false
            }
        );

        let refused = |text: &str| {
            let loaded = parse(&format!("schema_version = 1\n{text}")).unwrap();
            apply(&loaded.file, &mut EngineConfig::new("/state")).unwrap_err()
        };
        assert!(refused("[timeouts]\nconnect_seconds = 0").contains("connect_seconds"));
        assert!(refused("[timeouts]\nconnect_seconds = 90").contains("response_seconds"));
        assert!(refused("[downloads.max_active]\nlocked = true").contains("needs a `default`"));
        assert!(refused("[downloads.max_active]\nmax = 65").contains("downloads.max_active"));
        assert!(refused("[downloads.max_active]\ndefault = 9\nmax = 8").contains("default 9"));
    }
}
