//! The `[limits]` ceilings: their units, where each value comes from, and the precedence between
//! those places.
//!
//! A ceiling is set by a flag, which beats its `SUPDOC_*` variable, which beats the nearest config
//! in the `extends` chain, which beats the built-in default. `0` means no limit. Every other value
//! carries a unit (`ms`, `s`, `m` for time; `B`, `KiB`, `MiB` for size), so `10` is refused rather
//! than guessed as seconds or bytes.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::path::AbsolutePath;

/// One KiB in bytes.
const KIB: u64 = 1024;

/// One MiB in bytes.
const MIB: u64 = 1024 * KIB;

/// A time ceiling: how long one read of a file may take. `None` is no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeLimit(Option<Duration>);

impl TimeLimit {
    /// The ceiling, or `None` when there is no limit.
    #[must_use]
    pub const fn get(self) -> Option<Duration> {
        self.0
    }
}

impl FromStr for TimeLimit {
    type Err = LimitError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (number, unit) = split(text, Kind::Time)?;
        let millis = match unit {
            "ms" => Some(number),
            "s" => number.checked_mul(1000),
            "m" => number.checked_mul(60_000),
            _ => return Err(LimitError::new(text, Kind::Time)),
        }
        .ok_or_else(|| LimitError::new(text, Kind::Time))?;
        Ok(Self((millis != 0).then(|| Duration::from_millis(millis))))
    }
}

impl fmt::Display for TimeLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(duration) = self.0 else {
            return f.write_str("0");
        };
        let millis = duration.as_millis();
        if millis % 60_000 == 0 {
            write!(f, "{}m", millis / 60_000)
        } else if millis % 1000 == 0 {
            write!(f, "{}s", millis / 1000)
        } else {
            write!(f, "{millis}ms")
        }
    }
}

/// A size ceiling in bytes. `None` is no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeLimit(Option<u64>);

impl SizeLimit {
    /// The ceiling in bytes, or `None` when there is no limit.
    #[must_use]
    pub const fn get(self) -> Option<u64> {
        self.0
    }
}

impl FromStr for SizeLimit {
    type Err = LimitError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (number, unit) = split(text, Kind::Size)?;
        let bytes = match unit {
            "B" => Some(number),
            "KiB" => number.checked_mul(KIB),
            "MiB" => number.checked_mul(MIB),
            _ => return Err(LimitError::new(text, Kind::Size)),
        }
        .ok_or_else(|| LimitError::new(text, Kind::Size))?;
        Ok(Self((bytes != 0).then_some(bytes)))
    }
}

impl fmt::Display for SizeLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("0"),
            Some(bytes) if bytes % MIB == 0 => write!(f, "{}MiB", bytes / MIB),
            Some(bytes) if bytes % KIB == 0 => write!(f, "{}KiB", bytes / KIB),
            Some(bytes) => write!(f, "{bytes}B"),
        }
    }
}

/// Whether a ceiling measures time or size, which decides its units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `ms`, `s` or `m`.
    Time,
    /// `B`, `KiB` or `MiB`.
    Size,
}

/// Splits `text` into its number and its unit. A bare `0` is no limit, so it gets the unit that
/// keeps it zero.
///
/// # Errors
///
/// A [`LimitError`] when there is no number, or a non-zero number has no unit.
fn split(text: &str, kind: Kind) -> Result<(u64, &str), LimitError> {
    let text = text.trim();
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let number: u64 = number.parse().map_err(|_| LimitError::new(text, kind))?;
    match (number, unit) {
        (0, "") => Ok((0, if kind == Kind::Time { "ms" } else { "B" })),
        (_, "") => Err(LimitError::new(text, kind)),
        _ => Ok((number, unit)),
    }
}

/// A limit value that isn't `0` or a number with a known unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitError {
    /// The value as written.
    text: String,
    /// Which units would have been accepted.
    kind: Kind,
}

impl LimitError {
    /// The error for `text`, a value for a ceiling of `kind`.
    fn new(text: &str, kind: Kind) -> Self {
        Self {
            text: text.trim().to_owned(),
            kind,
        }
    }
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits: String = self.text.chars().take_while(char::is_ascii_digit).collect();
        let number = if digits.is_empty() { "10" } else { &digits };
        match self.kind {
            Kind::Time => write!(
                f,
                "`{}` is not a duration; give a unit, such as `{number}s` (also `ms` or `m`), or `0` for no limit",
                self.text
            ),
            Kind::Size => write!(
                f,
                "`{}` is not a size; give a unit, such as `{number}KiB` (also `B` or `MiB`), or `0` for no limit",
                self.text
            ),
        }
    }
}

impl std::error::Error for LimitError {}

/// Deserializes a limit from a TOML string, or the integer `0`, through its [`FromStr`].
///
/// # Errors
///
/// The deserializer's error for a value that is neither text nor an integer, or that
/// [`FromStr`] refuses.
fn deserialize_limit<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: FromStr<Err = LimitError>,
{
    /// Accepts a string, or an integer as the text it would be written as.
    struct Text;

    impl serde::de::Visitor<'_> for Text {
        type Value = String;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a value with a unit, such as \"10s\" or \"256KiB\", or 0 for no limit")
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
            Ok(value.to_owned())
        }

        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<String, E> {
            Ok(value.to_string())
        }

        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<String, E> {
            Ok(value.to_string())
        }
    }

    deserializer
        .deserialize_any(Text)?
        .parse()
        .map_err(serde::de::Error::custom)
}

impl<'de> serde::Deserialize<'de> for TimeLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_limit(deserializer)
    }
}

impl<'de> serde::Deserialize<'de> for SizeLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_limit(deserializer)
    }
}

/// Where a ceiling's value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Its command-line flag.
    Flag,
    /// Its `SUPDOC_*` environment variable.
    Env,
    /// The nearest config file in the `extends` chain that sets it.
    Config(AbsolutePath),
    /// The built-in default.
    Default,
}

/// A value given on the command line, through the flag itself or its environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Given<T> {
    /// The value.
    pub value: T,
    /// `true` when the environment variable gave it, `false` for the flag.
    pub from_env: bool,
}

/// The ceilings the command line gives, each through its flag or its variable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommandLineLimits {
    /// `--parse-timeout` or `SUPDOC_PARSE_TIMEOUT`.
    pub parse_timeout: Option<Given<TimeLimit>>,
    /// `--max-lock-size` or `SUPDOC_MAX_LOCK_SIZE`.
    pub max_lock_size: Option<Given<SizeLimit>>,
    /// `--max-config-size` or `SUPDOC_MAX_CONFIG_SIZE`. No config key sets it, since it must apply
    /// before a config file is parsed.
    pub max_config_size: Option<Given<SizeLimit>>,
    /// `--max-ignore-size` or `SUPDOC_MAX_IGNORE_SIZE`.
    pub max_ignore_size: Option<Given<SizeLimit>>,
}

/// The ceilings the config chain sets, each from the nearest file that sets it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLimits {
    /// `parse-timeout` and the file that set it.
    pub parse_timeout: Option<(TimeLimit, AbsolutePath)>,
    /// `max-lock-size` and the file that set it.
    pub max_lock_size: Option<(SizeLimit, AbsolutePath)>,
    /// `max-ignore-size` and the file that set it.
    pub max_ignore_size: Option<(SizeLimit, AbsolutePath)>,
}

/// One ceiling in effect and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting<T> {
    /// The ceiling.
    pub value: T,
    /// Where it came from.
    pub source: Source,
}

/// Every ceiling in effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// How long one read of a file may take to parse and query. A file read both as a source and
    /// as a target gets this budget for each read.
    pub parse_timeout: Setting<TimeLimit>,
    /// How large `supdoc.lock` may be.
    pub max_lock_size: Setting<SizeLimit>,
    /// How large each config file in the `extends` chain may be.
    pub max_config_size: Setting<SizeLimit>,
    /// How large each ignore file may be.
    pub max_ignore_size: Setting<SizeLimit>,
}

impl Limits {
    /// The default time a file may take to read: 10 s.
    pub const PARSE_TIMEOUT: TimeLimit = TimeLimit(Some(Duration::from_secs(10)));
    /// The default lock size ceiling: 16 MiB.
    pub const MAX_LOCK_SIZE: SizeLimit = SizeLimit(Some(16 * MIB));
    /// The default config size ceiling: 256 KiB.
    pub const MAX_CONFIG_SIZE: SizeLimit = SizeLimit(Some(256 * KIB));
    /// The default ignore file size ceiling: 256 KiB.
    pub const MAX_IGNORE_SIZE: SizeLimit = SizeLimit(Some(256 * KIB));

    /// Each ceiling from the command line, else the config, else its default.
    #[must_use]
    pub fn resolve(command_line: &CommandLineLimits, config: &ConfigLimits) -> Self {
        Self {
            parse_timeout: pick(
                command_line.parse_timeout,
                config.parse_timeout.as_ref(),
                Self::PARSE_TIMEOUT,
            ),
            max_lock_size: pick(
                command_line.max_lock_size,
                config.max_lock_size.as_ref(),
                Self::MAX_LOCK_SIZE,
            ),
            max_config_size: pick(command_line.max_config_size, None, Self::MAX_CONFIG_SIZE),
            max_ignore_size: pick(
                command_line.max_ignore_size,
                config.max_ignore_size.as_ref(),
                Self::MAX_IGNORE_SIZE,
            ),
        }
    }
}

/// The first of `given`, `config` and `default` that exists, with where it came from.
fn pick<T: Copy>(
    given: Option<Given<T>>,
    config: Option<&(T, AbsolutePath)>,
    default: T,
) -> Setting<T> {
    match (given, config) {
        (Some(given), _) => Setting {
            value: given.value,
            source: if given.from_env {
                Source::Env
            } else {
                Source::Flag
            },
        },
        (None, Some((value, file))) => Setting {
            value: *value,
            source: Source::Config(file.clone()),
        },
        (None, None) => Setting {
            value: default,
            source: Source::Default,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn time(text: &str) -> Result<TimeLimit, LimitError> {
        text.parse()
    }

    fn size(text: &str) -> Result<SizeLimit, LimitError> {
        text.parse()
    }

    #[test]
    fn units_parse_and_print_in_their_largest_whole_unit() {
        assert_eq!(time("500ms").map(|t| t.to_string()), Ok("500ms".to_owned()));
        assert_eq!(time("120s").map(|t| t.to_string()), Ok("2m".to_owned()));
        assert_eq!(
            size("1024KiB").map(|s| s.to_string()),
            Ok("1MiB".to_owned())
        );
        assert_eq!(size("300B").map(|s| s.to_string()), Ok("300B".to_owned()));
    }

    #[test]
    fn zero_means_no_limit_with_or_without_a_unit() {
        assert_eq!(time("0").map(TimeLimit::get), Ok(None));
        assert_eq!(time("0s").map(TimeLimit::get), Ok(None));
        assert_eq!(size("0").map(SizeLimit::get), Ok(None));
    }

    #[test]
    fn a_bare_nonzero_number_is_refused_with_the_unit_form() {
        let error = time("7").expect_err("a bare 7 should be refused");
        assert!(error.to_string().contains("`7s`"), "{error}");
        let error = size("300").expect_err("a bare 300 should be refused");
        assert!(error.to_string().contains("`300KiB`"), "{error}");
        assert!(time("7h").is_err(), "an unknown unit should be refused");
        assert!(
            size("kib").is_err(),
            "a unit without a number should be refused"
        );
    }

    #[test]
    fn flag_beats_env_beats_config_beats_default() {
        let file = AbsolutePath::new(Path::new(std::path::MAIN_SEPARATOR_STR))
            .expect("the root should be absolute");
        let config = ConfigLimits {
            parse_timeout: Some((time("30s").expect("30s"), file.clone())),
            max_lock_size: Some((size("1MiB").expect("1MiB"), file.clone())),
            max_ignore_size: None,
        };
        let command_line = CommandLineLimits {
            parse_timeout: Some(Given {
                value: time("5s").expect("5s"),
                from_env: true,
            }),
            ..CommandLineLimits::default()
        };

        let limits = Limits::resolve(&command_line, &config);

        assert_eq!(limits.parse_timeout.source, Source::Env);
        assert_eq!(limits.parse_timeout.value.to_string(), "5s");
        assert_eq!(limits.max_lock_size.source, Source::Config(file));
        assert_eq!(limits.max_ignore_size.source, Source::Default);
        assert_eq!(limits.max_config_size.value, Limits::MAX_CONFIG_SIZE);
    }
}
