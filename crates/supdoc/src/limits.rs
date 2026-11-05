//! The `[limits]` ceilings: their units, where each value comes from, and the precedence between
//! those places.
//!
//! A ceiling is set by a flag, which beats its `SUPDOC_*` variable, which beats the nearest config
//! in the `extends` chain, which beats the built-in default. `0` means no limit. Every other value
//! carries a unit (`ms`, `s`, `m` for time; `B`, `KiB`, `MiB` for size), so `10` is refused rather
//! than guessed as seconds or bytes.
//!
//! A config may lower a ceiling in any checkout, but raises it (or lifts it with `0`) only in a
//! checkout the operator trusts: the checkout chose that file, so it can't free itself of a
//! ceiling. Otherwise the default applies and the [`Setting`] records the value held back.

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
pub(crate) struct TimeLimit(Option<Duration>);

impl TimeLimit {
    /// The ceiling, or `None` when there is no limit.
    #[must_use]
    pub(crate) const fn get(self) -> Option<Duration> {
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
pub(crate) struct SizeLimit(Option<u64>);

impl SizeLimit {
    /// The ceiling of `bytes` bytes; `0` is no limit, as when it is written.
    #[must_use]
    pub(crate) const fn bytes(bytes: u64) -> Self {
        Self(if bytes == 0 { None } else { Some(bytes) })
    }

    /// The ceiling in bytes, or `None` when there is no limit.
    #[must_use]
    pub(crate) const fn get(self) -> Option<u64> {
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

/// A count ceiling: how many of something one read of a file may make. `None` is no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CountLimit(Option<u64>);

impl CountLimit {
    /// The ceiling, or `None` when there is no limit.
    #[must_use]
    pub(crate) const fn get(self) -> Option<u64> {
        self.0
    }
}

impl FromStr for CountLimit {
    type Err = LimitError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let number: u64 = text
            .trim()
            .parse()
            .map_err(|_| LimitError::new(text, Kind::Count))?;
        Ok(Self((number != 0).then_some(number)))
    }
}

impl fmt::Display for CountLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("0"),
            Some(count) => write!(f, "{count}"),
        }
    }
}

/// A ceiling that can be compared with its default by what it measures, not by how it is written.
trait Ceiling: Copy {
    /// Whether `self` allows more than `default`. No limit counts as infinite, so `0` raises any
    /// finite default, and `16384KiB` doesn't raise `16MiB`.
    fn raises(self, default: Self) -> bool;
}

/// Whether the ceiling `value` allows more than `default`, where `None` is no limit.
fn raises<T: Ord>(value: Option<T>, default: Option<T>) -> bool {
    match (value, default) {
        (None, Some(_)) => true,
        (Some(value), Some(default)) => value > default,
        (_, None) => false,
    }
}

impl Ceiling for TimeLimit {
    fn raises(self, default: Self) -> bool {
        raises(self.0, default.0)
    }
}

impl Ceiling for SizeLimit {
    fn raises(self, default: Self) -> bool {
        raises(self.0, default.0)
    }
}

impl Ceiling for CountLimit {
    fn raises(self, default: Self) -> bool {
        raises(self.0, default.0)
    }
}

/// Whether a ceiling measures time, size or a count, which decides its units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `ms`, `s` or `m`.
    Time,
    /// `B`, `KiB` or `MiB`.
    Size,
    /// A whole number with no unit.
    Count,
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
pub(crate) struct LimitError {
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
        let number = match (digits.is_empty(), self.kind) {
            (false, _) => digits.as_str(),
            (true, Kind::Count) => "200000",
            (true, _) => "10",
        };
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
            Kind::Count => write!(
                f,
                "`{}` is not a count; give a whole number with no unit, such as `{number}`, or `0` for no limit",
                self.text
            ),
        }
    }
}

impl std::error::Error for LimitError {}

/// What a time or size limit's value is expected to be, in serde's message for a value of the
/// wrong type.
const EXPECTING_UNIT: &str =
    "a value with a unit, such as \"10s\" or \"256KiB\", or 0 for no limit";

/// What a count limit's value is expected to be, in serde's message for a value of the wrong type.
const EXPECTING_COUNT: &str = "a whole number with no unit, such as 200000, or 0 for no limit";

/// Deserializes a limit from a TOML string, or an integer, through its [`FromStr`]. `expecting`
/// names the values it takes when the value is of another type.
///
/// # Errors
///
/// The deserializer's error for a value that is neither text nor an integer, or that
/// [`FromStr`] refuses.
fn deserialize_limit<'de, D, T>(deserializer: D, expecting: &'static str) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: FromStr<Err = LimitError>,
{
    /// Accepts a string, or an integer as the text it would be written as.
    struct Text(&'static str);

    impl serde::de::Visitor<'_> for Text {
        type Value = String;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
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
        .deserialize_any(Text(expecting))?
        .parse()
        .map_err(serde::de::Error::custom)
}

impl<'de> serde::Deserialize<'de> for TimeLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_limit(deserializer, EXPECTING_UNIT)
    }
}

impl<'de> serde::Deserialize<'de> for SizeLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_limit(deserializer, EXPECTING_UNIT)
    }
}

impl<'de> serde::Deserialize<'de> for CountLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_limit(deserializer, EXPECTING_COUNT)
    }
}

/// Where a ceiling's value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
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
pub(crate) struct Given<T> {
    /// The value.
    pub(crate) value: T,
    /// `true` when the environment variable gave it, `false` for the flag.
    pub(crate) from_env: bool,
}

/// The ceilings the command line gives, each through its flag or its variable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CommandLineLimits {
    /// `--parse-timeout` or `SUPDOC_PARSE_TIMEOUT`.
    pub(crate) parse_timeout: Option<Given<TimeLimit>>,
    /// `--code-block-timeout` or `SUPDOC_CODE_BLOCK_TIMEOUT`.
    pub(crate) code_block_timeout: Option<Given<TimeLimit>>,
    /// `--code-block-build-timeout` or `SUPDOC_CODE_BLOCK_BUILD_TIMEOUT`.
    pub(crate) code_block_build_timeout: Option<Given<TimeLimit>>,
    /// `--max-file-size` or `SUPDOC_MAX_FILE_SIZE`.
    pub(crate) max_file_size: Option<Given<SizeLimit>>,
    /// `--max-lock-size` or `SUPDOC_MAX_LOCK_SIZE`.
    pub(crate) max_lock_size: Option<Given<SizeLimit>>,
    /// `--max-config-size` or `SUPDOC_MAX_CONFIG_SIZE`. No config key sets it, since it must apply
    /// before a config file is parsed.
    pub(crate) max_config_size: Option<Given<SizeLimit>>,
    /// `--max-ignore-size` or `SUPDOC_MAX_IGNORE_SIZE`.
    pub(crate) max_ignore_size: Option<Given<SizeLimit>>,
    /// `--max-key-segments` or `SUPDOC_MAX_KEY_SEGMENTS`.
    pub(crate) max_key_segments: Option<Given<CountLimit>>,
    /// `--max-key-path-size` or `SUPDOC_MAX_KEY_PATH_SIZE`.
    pub(crate) max_key_path_size: Option<Given<SizeLimit>>,
}

/// The ceilings the config chain sets, each from the nearest file that sets it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConfigLimits {
    /// `parse-timeout` and the file that set it.
    pub(crate) parse_timeout: Option<(TimeLimit, AbsolutePath)>,
    /// `code-block-timeout` and the file that set it.
    pub(crate) code_block_timeout: Option<(TimeLimit, AbsolutePath)>,
    /// `code-block-build-timeout` and the file that set it.
    pub(crate) code_block_build_timeout: Option<(TimeLimit, AbsolutePath)>,
    /// `max-file-size` and the file that set it.
    pub(crate) max_file_size: Option<(SizeLimit, AbsolutePath)>,
    /// `max-lock-size` and the file that set it.
    pub(crate) max_lock_size: Option<(SizeLimit, AbsolutePath)>,
    /// `max-ignore-size` and the file that set it.
    pub(crate) max_ignore_size: Option<(SizeLimit, AbsolutePath)>,
    /// `max-key-segments` and the file that set it.
    pub(crate) max_key_segments: Option<(CountLimit, AbsolutePath)>,
    /// `max-key-path-size` and the file that set it.
    pub(crate) max_key_path_size: Option<(SizeLimit, AbsolutePath)>,
}

/// One ceiling in effect and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Setting<T> {
    /// The ceiling.
    pub(crate) value: T,
    /// Where it came from.
    pub(crate) source: Source,
    /// The config's raise of this ceiling, when the checkout isn't trusted and so the default
    /// applies instead. `None` whenever the config's value took effect or no config set one.
    pub(crate) held_back: Option<HeldBack<T>>,
}

/// A config value that would raise a ceiling, held back because the checkout isn't trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeldBack<T> {
    /// The value the config sets.
    pub(crate) value: T,
    /// The config file that sets it.
    pub(crate) file: AbsolutePath,
}

/// Whether a config value that raises a ceiling above its default takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Raises {
    /// The checkout isn't trusted: the default applies, and the raise is recorded as held back.
    Held,
    /// The operator trusts the checkout: the raise takes effect.
    Honored,
}

/// Every ceiling in effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Limits {
    /// How long one read of a file may take to parse and query. A file read both as a source and
    /// as a target gets this budget for each read.
    pub(crate) parse_timeout: Setting<TimeLimit>,
    /// How long one code block may run. `0` means no limit.
    pub(crate) code_block_timeout: Setting<TimeLimit>,
    /// How long building the project for a code block's lint may take. `0` means no limit.
    pub(crate) code_block_build_timeout: Setting<TimeLimit>,
    /// How large a doc or linked file supdoc reads may be.
    pub(crate) max_file_size: Setting<SizeLimit>,
    /// How large `supdoc.lock` may be.
    pub(crate) max_lock_size: Setting<SizeLimit>,
    /// How large each config file in the `extends` chain may be.
    pub(crate) max_config_size: Setting<SizeLimit>,
    /// How large each ignore file may be.
    pub(crate) max_ignore_size: Setting<SizeLimit>,
    /// How many path segments all the symbols of one data file (JSON, YAML, TOML) may hold
    /// together.
    pub(crate) max_key_segments: Setting<CountLimit>,
    /// How many bytes of path text all the symbols of one data file may hold together.
    pub(crate) max_key_path_size: Setting<SizeLimit>,
}

impl Limits {
    /// The default time a file may take to read: 10 s.
    pub(crate) const PARSE_TIMEOUT: TimeLimit = TimeLimit(Some(Duration::from_secs(10)));
    /// The default time one code block may run: 10 s.
    pub(crate) const CODE_BLOCK_TIMEOUT: TimeLimit = TimeLimit(Some(Duration::from_secs(10)));
    /// The default time building the project for a lint may take: 10 min.
    pub(crate) const CODE_BLOCK_BUILD_TIMEOUT: TimeLimit =
        TimeLimit(Some(Duration::from_secs(600)));
    /// The default ceiling on a doc or linked file: 16 MiB. Parsing a file can take a few hundred
    /// times its size in memory.
    ///
    /// Changing it also changes the `--max-file-size` help example ("such as 16MiB") in
    /// `cli/args.rs` and the `max-file-size` row of the `config show` golden
    /// (`tests/output/snapshots/output__goldens__config_show.snap`).
    pub(crate) const MAX_FILE_SIZE: SizeLimit = SizeLimit(Some(16 * MIB));
    /// The default lock size ceiling: 16 MiB.
    pub(crate) const MAX_LOCK_SIZE: SizeLimit = SizeLimit(Some(16 * MIB));
    /// The default config size ceiling: 256 KiB.
    pub(crate) const MAX_CONFIG_SIZE: SizeLimit = SizeLimit(Some(256 * KIB));
    /// The default ignore file size ceiling: 256 KiB.
    pub(crate) const MAX_IGNORE_SIZE: SizeLimit = SizeLimit(Some(256 * KIB));
    /// The default key-path segment ceiling: 200,000.
    ///
    /// A symbol costs about 415 bytes of table at one segment each (measured at 415 MB for
    /// 1,000,000 one-byte keys), so this keeps the worst-case file near 81 MB, under the roughly
    /// 100 MB the design allows. Cargo.toml and a 3,000-line API spec are well under 50,000
    /// segments. A 1.66 MB `package-lock.json` measured 115,748, so a lockfile over about 2.9 MB
    /// exceeds this default.
    pub(crate) const MAX_KEY_SEGMENTS: CountLimit = CountLimit(Some(200_000));
    /// The default key-path text ceiling: 32 MiB.
    pub(crate) const MAX_KEY_PATH_SIZE: SizeLimit = SizeLimit(Some(32 * MIB));

    /// Each ceiling from the command line, else the config, else its default, with every config
    /// raise held back: the result for a checkout that isn't trusted.
    #[must_use]
    pub(crate) fn resolve(command_line: &CommandLineLimits, config: &ConfigLimits) -> Self {
        Self::resolve_with(command_line, config, Raises::Held)
    }

    /// Each ceiling from the command line, else the config, else its default. A config value above
    /// its default takes effect only when `raises` is [`Raises::Honored`]; the command line is
    /// never held back.
    #[must_use]
    pub(crate) fn resolve_with(
        command_line: &CommandLineLimits,
        config: &ConfigLimits,
        raises: Raises,
    ) -> Self {
        Self {
            parse_timeout: pick(
                command_line.parse_timeout,
                config.parse_timeout.as_ref(),
                Self::PARSE_TIMEOUT,
                raises,
            ),
            code_block_timeout: pick(
                command_line.code_block_timeout,
                config.code_block_timeout.as_ref(),
                Self::CODE_BLOCK_TIMEOUT,
                raises,
            ),
            code_block_build_timeout: pick(
                command_line.code_block_build_timeout,
                config.code_block_build_timeout.as_ref(),
                Self::CODE_BLOCK_BUILD_TIMEOUT,
                raises,
            ),
            max_file_size: pick(
                command_line.max_file_size,
                config.max_file_size.as_ref(),
                Self::MAX_FILE_SIZE,
                raises,
            ),
            max_lock_size: pick(
                command_line.max_lock_size,
                config.max_lock_size.as_ref(),
                Self::MAX_LOCK_SIZE,
                raises,
            ),
            max_config_size: pick(
                command_line.max_config_size,
                None,
                Self::MAX_CONFIG_SIZE,
                raises,
            ),
            max_ignore_size: pick(
                command_line.max_ignore_size,
                config.max_ignore_size.as_ref(),
                Self::MAX_IGNORE_SIZE,
                raises,
            ),
            max_key_segments: pick(
                command_line.max_key_segments,
                config.max_key_segments.as_ref(),
                Self::MAX_KEY_SEGMENTS,
                raises,
            ),
            max_key_path_size: pick(
                command_line.max_key_path_size,
                config.max_key_path_size.as_ref(),
                Self::MAX_KEY_PATH_SIZE,
                raises,
            ),
        }
    }

    /// Whether a config raise of any ceiling was held back, so trusting the checkout would change
    /// a ceiling.
    #[must_use]
    pub(crate) fn holds_back(&self) -> bool {
        self.parse_timeout.held_back.is_some()
            || self.code_block_timeout.held_back.is_some()
            || self.code_block_build_timeout.held_back.is_some()
            || self.max_file_size.held_back.is_some()
            || self.max_lock_size.held_back.is_some()
            || self.max_config_size.held_back.is_some()
            || self.max_ignore_size.held_back.is_some()
            || self.max_key_segments.held_back.is_some()
            || self.max_key_path_size.held_back.is_some()
    }
}

/// The first of `given`, `config` and `default` that exists, with where it came from. A `config`
/// value above `default` is held back unless `raises` honors it.
fn pick<T: Ceiling>(
    given: Option<Given<T>>,
    config: Option<&(T, AbsolutePath)>,
    default: T,
    raises: Raises,
) -> Setting<T> {
    match (given, config) {
        (Some(given), _) => Setting {
            value: given.value,
            source: if given.from_env {
                Source::Env
            } else {
                Source::Flag
            },
            held_back: None,
        },
        (None, Some((value, file))) if value.raises(default) && raises == Raises::Held => Setting {
            value: default,
            source: Source::Default,
            held_back: Some(HeldBack {
                value: *value,
                file: file.clone(),
            }),
        },
        (None, Some((value, file))) => Setting {
            value: *value,
            source: Source::Config(file.clone()),
            held_back: None,
        },
        (None, None) => Setting {
            value: default,
            source: Source::Default,
            held_back: None,
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
            code_block_timeout: Some((time("20s").expect("20s"), file.clone())),
            code_block_build_timeout: None,
            max_file_size: None,
            max_lock_size: Some((size("1MiB").expect("1MiB"), file.clone())),
            max_ignore_size: None,
            max_key_segments: None,
            max_key_path_size: None,
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

    #[test]
    fn code_block_timeout_defaults_to_ten_seconds_and_zero_means_no_limit() {
        let limits = Limits::resolve(&CommandLineLimits::default(), &ConfigLimits::default());
        assert_eq!(limits.code_block_timeout.value.to_string(), "10s");
        assert_eq!(limits.code_block_timeout.source, Source::Default);
        assert_eq!(time("0").map(TimeLimit::get), Ok(None));
    }

    #[test]
    fn code_block_timeout_takes_flag_then_env_then_config() {
        let file = AbsolutePath::new(Path::new(std::path::MAIN_SEPARATOR_STR))
            .expect("the root should be absolute");
        let config = ConfigLimits {
            code_block_timeout: Some((time("3s").expect("3s"), file.clone())),
            ..ConfigLimits::default()
        };
        let from_config = Limits::resolve(&CommandLineLimits::default(), &config);
        assert_eq!(from_config.code_block_timeout.source, Source::Config(file));
        let env = CommandLineLimits {
            code_block_timeout: Some(Given {
                value: time("5s").expect("5s"),
                from_env: true,
            }),
            ..CommandLineLimits::default()
        };
        let from_env = Limits::resolve(&env, &config);
        assert_eq!(from_env.code_block_timeout.source, Source::Env);
        assert_eq!(from_env.code_block_timeout.value.to_string(), "5s");
    }

    #[test]
    fn code_block_build_timeout_defaults_to_ten_minutes_and_zero_means_no_limit() {
        let limits = Limits::resolve(&CommandLineLimits::default(), &ConfigLimits::default());
        assert_eq!(limits.code_block_build_timeout.value.to_string(), "10m");
        assert_eq!(limits.code_block_build_timeout.source, Source::Default);
        assert_eq!(time("0").map(TimeLimit::get), Ok(None));
    }

    #[test]
    fn code_block_build_timeout_takes_flag_then_env_then_config() {
        let file = AbsolutePath::new(Path::new(std::path::MAIN_SEPARATOR_STR))
            .expect("the root should be absolute");
        let config = ConfigLimits {
            code_block_build_timeout: Some((time("20s").expect("20s"), file.clone())),
            ..ConfigLimits::default()
        };
        let from_config = Limits::resolve(&CommandLineLimits::default(), &config);
        assert_eq!(
            from_config.code_block_build_timeout.source,
            Source::Config(file)
        );
        let env = CommandLineLimits {
            code_block_build_timeout: Some(Given {
                value: time("5s").expect("5s"),
                from_env: true,
            }),
            ..CommandLineLimits::default()
        };
        let from_env = Limits::resolve(&env, &config);
        assert_eq!(from_env.code_block_build_timeout.source, Source::Env);
        assert_eq!(from_env.code_block_build_timeout.value.to_string(), "5s");
    }

    #[test]
    fn max_file_size_takes_flag_then_env_then_config_then_its_default() {
        let file = AbsolutePath::new(Path::new(std::path::MAIN_SEPARATOR_STR))
            .expect("the root should be absolute");
        let defaults = Limits::resolve(&CommandLineLimits::default(), &ConfigLimits::default());
        assert_eq!(defaults.max_file_size.value, Limits::MAX_FILE_SIZE);
        assert_eq!(defaults.max_file_size.source, Source::Default);
        let config = ConfigLimits {
            max_file_size: Some((size("1MiB").expect("1MiB"), file.clone())),
            ..ConfigLimits::default()
        };
        let from_config = Limits::resolve(&CommandLineLimits::default(), &config);
        assert_eq!(from_config.max_file_size.source, Source::Config(file));
        let given = |text: &str, from_env| CommandLineLimits {
            max_file_size: Some(Given {
                value: size(text).expect("a size with a unit"),
                from_env,
            }),
            ..CommandLineLimits::default()
        };
        let from_env = Limits::resolve(&given("2MiB", true), &config);
        assert_eq!(from_env.max_file_size.source, Source::Env);
        assert_eq!(from_env.max_file_size.value.to_string(), "2MiB");
        let from_flag = Limits::resolve(&given("0", false), &config);
        assert_eq!(from_flag.max_file_size.source, Source::Flag);
        assert_eq!(
            from_flag.max_file_size.value.get(),
            None,
            "0 lifts the limit"
        );
        assert!(
            size("4").is_err(),
            "a size without a unit should be refused"
        );
    }

    #[test]
    fn a_count_is_a_bare_number_and_zero_means_no_limit() {
        let count = |text: &str| text.parse::<CountLimit>();
        assert_eq!(count("200000").map(CountLimit::get), Ok(Some(200_000)));
        assert_eq!(count("0").map(CountLimit::get), Ok(None));
        let error = count("5KiB").expect_err("a count with a unit should be refused");
        assert!(error.to_string().contains("whole number"), "{error}");
        assert!(count("").is_err(), "an empty count should be refused");
    }

    #[test]
    fn the_key_path_limits_take_flag_then_env_then_config_then_default() {
        let file = AbsolutePath::new(Path::new(std::path::MAIN_SEPARATOR_STR))
            .expect("the root should be absolute");
        let config = ConfigLimits {
            max_key_segments: Some((CountLimit(Some(5)), file.clone())),
            max_key_path_size: Some((size("1MiB").expect("1MiB"), file.clone())),
            ..ConfigLimits::default()
        };
        let from_config = Limits::resolve(&CommandLineLimits::default(), &config);
        assert_eq!(from_config.max_key_segments.source, Source::Config(file));
        assert_eq!(from_config.max_key_path_size.value.to_string(), "1MiB");
        let command_line = CommandLineLimits {
            max_key_segments: Some(Given {
                value: CountLimit(Some(9)),
                from_env: true,
            }),
            max_key_path_size: Some(Given {
                value: size("2MiB").expect("2MiB"),
                from_env: false,
            }),
            ..CommandLineLimits::default()
        };
        let from_command_line = Limits::resolve(&command_line, &config);
        assert_eq!(from_command_line.max_key_segments.source, Source::Env);
        assert_eq!(from_command_line.max_key_path_size.source, Source::Flag);
        let defaults = Limits::resolve(&CommandLineLimits::default(), &ConfigLimits::default());
        assert_eq!(defaults.max_key_segments.value.to_string(), "200000");
        assert_eq!(defaults.max_key_path_size.value.to_string(), "32MiB");
        assert_eq!(defaults.max_key_segments.source, Source::Default);
    }

    /// The eight config-settable keys, in `config show` order.
    const CONFIG_KEYS: [&str; 8] = [
        "parse-timeout",
        "code-block-timeout",
        "code-block-build-timeout",
        "max-file-size",
        "max-lock-size",
        "max-ignore-size",
        "max-key-segments",
        "max-key-path-size",
    ];

    /// A config file to attribute config values to.
    fn config_file() -> AbsolutePath {
        AbsolutePath::new(Path::new(std::path::MAIN_SEPARATOR_STR))
            .expect("the root should be absolute")
    }

    /// A config that sets every key in [`CONFIG_KEYS`] to the value at the same index.
    fn config_setting(values: [&str; 8]) -> ConfigLimits {
        let [
            parse,
            block,
            build,
            file_size,
            lock,
            ignore,
            segments,
            key_path,
        ] = values;
        let time = |text| Some((time(text).expect("a duration"), config_file()));
        let size = |text| Some((size(text).expect("a size"), config_file()));
        ConfigLimits {
            parse_timeout: time(parse),
            code_block_timeout: time(block),
            code_block_build_timeout: time(build),
            max_file_size: size(file_size),
            max_lock_size: size(lock),
            max_ignore_size: size(ignore),
            max_key_segments: Some((segments.parse().expect("a count"), config_file())),
            max_key_path_size: size(key_path),
        }
    }

    /// One config-settable key as resolved: its value, source and held-back value, as text.
    type Row = (&'static str, String, Source, Option<String>);

    /// The row of `setting` under `key`.
    fn row<T: fmt::Display>(key: &'static str, setting: &Setting<T>) -> Row {
        (
            key,
            setting.value.to_string(),
            setting.source.clone(),
            setting
                .held_back
                .as_ref()
                .map(|held| held.value.to_string()),
        )
    }

    /// Every config-settable key of `limits`, in [`CONFIG_KEYS`] order.
    fn rows(limits: &Limits) -> Vec<Row> {
        vec![
            row(CONFIG_KEYS[0], &limits.parse_timeout),
            row(CONFIG_KEYS[1], &limits.code_block_timeout),
            row(CONFIG_KEYS[2], &limits.code_block_build_timeout),
            row(CONFIG_KEYS[3], &limits.max_file_size),
            row(CONFIG_KEYS[4], &limits.max_lock_size),
            row(CONFIG_KEYS[5], &limits.max_ignore_size),
            row(CONFIG_KEYS[6], &limits.max_key_segments),
            row(CONFIG_KEYS[7], &limits.max_key_path_size),
        ]
    }

    /// Every key's default, as text, in [`CONFIG_KEYS`] order.
    const DEFAULTS: [&str; 8] = [
        "10s", "10s", "10m", "16MiB", "16MiB", "256KiB", "200000", "32MiB",
    ];

    /// Each key one step above its default.
    const ABOVE: [&str; 8] = [
        "11s", "11s", "11m", "17MiB", "17MiB", "257KiB", "200001", "33MiB",
    ];

    /// Each key one step below its default.
    const BELOW: [&str; 8] = [
        "9s", "9s", "9m", "15MiB", "15MiB", "255KiB", "199999", "31MiB",
    ];

    /// Each key at its default, spelled in a smaller unit where it has one.
    const EQUAL: [&str; 8] = [
        "10000ms", "10000ms", "600s", "16384KiB", "16384KiB", "262144B", "200000", "32768KiB",
    ];

    /// Every key held back: the default in effect, with `held` recorded.
    fn held(held: [&str; 8]) -> Vec<Row> {
        CONFIG_KEYS
            .iter()
            .zip(DEFAULTS.iter().zip(held))
            .map(|(key, (default, held))| {
                (
                    *key,
                    (*default).to_owned(),
                    Source::Default,
                    Some(held.to_owned()),
                )
            })
            .collect()
    }

    /// Every key from the config, at `values` as printed, with nothing held back.
    fn honored(values: [&str; 8]) -> Vec<Row> {
        CONFIG_KEYS
            .iter()
            .zip(values)
            .map(|(key, value)| (*key, value.to_owned(), Source::Config(config_file()), None))
            .collect()
    }

    #[test]
    fn an_untrusted_config_lowers_every_ceiling_but_raises_or_lifts_none() {
        let untrusted = |values| {
            rows(&Limits::resolve(
                &CommandLineLimits::default(),
                &config_setting(values),
            ))
        };

        assert_eq!(untrusted(["0"; 8]), held(["0"; 8]), "0 lifts nothing");
        assert_eq!(untrusted(ABOVE), held(ABOVE), "a raise is held back");
        assert_eq!(untrusted(BELOW), honored(BELOW), "a lower ceiling applies");
        assert_eq!(
            untrusted(EQUAL),
            honored(DEFAULTS),
            "the default spelled in another unit is not a raise"
        );
    }

    #[test]
    fn a_trusted_config_raises_and_lifts_every_ceiling() {
        let trusted = |values| {
            rows(&Limits::resolve_with(
                &CommandLineLimits::default(),
                &config_setting(values),
                Raises::Honored,
            ))
        };

        assert_eq!(trusted(["0"; 8]), honored(["0"; 8]));
        assert_eq!(trusted(ABOVE), honored(ABOVE));
        assert_eq!(trusted(BELOW), honored(BELOW));
    }

    #[test]
    fn env_and_flag_beat_a_held_back_config_with_nothing_held_back() {
        let given = |from_env| {
            let time = |text| {
                Some(Given {
                    value: time(text).expect("a duration"),
                    from_env,
                })
            };
            let size = |text| {
                Some(Given {
                    value: size(text).expect("a size"),
                    from_env,
                })
            };
            CommandLineLimits {
                parse_timeout: time("1s"),
                code_block_timeout: time("1s"),
                code_block_build_timeout: time("0"),
                max_file_size: size("0"),
                max_lock_size: size("1MiB"),
                max_config_size: None,
                max_ignore_size: size("0"),
                max_key_segments: Some(Given {
                    value: CountLimit(None),
                    from_env,
                }),
                max_key_path_size: size("64MiB"),
            }
        };
        for raises in [Raises::Held, Raises::Honored] {
            for from_env in [true, false] {
                let limits =
                    Limits::resolve_with(&given(from_env), &config_setting(["0"; 8]), raises);
                let source = if from_env { Source::Env } else { Source::Flag };
                let expected: Vec<Row> = CONFIG_KEYS
                    .iter()
                    .zip(["1s", "1s", "0", "0", "1MiB", "0", "0", "64MiB"])
                    .map(|(key, value)| (*key, value.to_owned(), source.clone(), None))
                    .collect();
                assert_eq!(rows(&limits), expected, "{raises:?}, from_env {from_env}");
                assert!(!limits.holds_back(), "{raises:?}, from_env {from_env}");
            }
        }
    }

    #[test]
    fn holds_back_says_whether_any_raise_was_held_back() {
        let none = Limits::resolve(&CommandLineLimits::default(), &config_setting(BELOW));
        assert!(!none.holds_back(), "lowering holds nothing back");
        for (index, key) in CONFIG_KEYS.iter().enumerate() {
            let mut values = BELOW;
            *values.get_mut(index).expect("one value per key") = "0";

            let one = Limits::resolve(&CommandLineLimits::default(), &config_setting(values));

            assert!(one.holds_back(), "a lifted {key} alone is held back");
        }
    }
}
