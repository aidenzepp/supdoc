//! `[features.code-blocks.sql]`: the table that turns checking SQL code blocks on, tagged `sql`
//! or a dialect's own name (see [`SqlDialect::from_tag`]).
//!
//! A `sql` block is read in the table's `dialect`, `ansi` unless written. A block tagged with a
//! dialect's name is read in that dialect whatever `dialect` says: the tag is the author's
//! explicit choice for that block.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The SQL formatters supdoc knows.
const KNOWN: &[&str] = &["sqruff-format"];

/// The SQL linters supdoc knows.
const LINTERS: &[&str] = &["sqruff-lint"];

/// The SQL dialect sqruff parses a block as, named as sqruff names it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SqlDialect {
    /// ANSI SQL, the default.
    #[default]
    Ansi,
    /// Amazon Athena.
    Athena,
    /// Google `BigQuery`.
    Bigquery,
    /// `ClickHouse`.
    Clickhouse,
    /// Databricks.
    Databricks,
    /// IBM Db2.
    Db2,
    /// Apache Doris.
    Doris,
    /// `DuckDB`.
    Duckdb,
    /// Exasol.
    Exasol,
    /// Apache Flink SQL.
    Flink,
    /// Greenplum.
    Greenplum,
    /// Apache Hive.
    Hive,
    /// Apache Impala.
    Impala,
    /// `MariaDB`.
    Mariadb,
    /// Materialize.
    Materialize,
    /// `MySQL`.
    Mysql,
    /// Oracle.
    Oracle,
    /// `PostgreSQL`.
    Postgres,
    /// Amazon Redshift.
    Redshift,
    /// Snowflake.
    Snowflake,
    /// Spark SQL.
    Sparksql,
    /// `SQLite`.
    Sqlite,
    /// `StarRocks`.
    Starrocks,
    /// Teradata.
    Teradata,
    /// Trino.
    Trino,
    /// Transact-SQL (SQL Server).
    Tsql,
    /// Vertica.
    Vertica,
}

impl SqlDialect {
    /// The value of sqruff's `--dialect` flag.
    #[must_use]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Ansi => "ansi",
            Self::Athena => "athena",
            Self::Bigquery => "bigquery",
            Self::Clickhouse => "clickhouse",
            Self::Databricks => "databricks",
            Self::Db2 => "db2",
            Self::Doris => "doris",
            Self::Duckdb => "duckdb",
            Self::Exasol => "exasol",
            Self::Flink => "flink",
            Self::Greenplum => "greenplum",
            Self::Hive => "hive",
            Self::Impala => "impala",
            Self::Mariadb => "mariadb",
            Self::Materialize => "materialize",
            Self::Mysql => "mysql",
            Self::Oracle => "oracle",
            Self::Postgres => "postgres",
            Self::Redshift => "redshift",
            Self::Snowflake => "snowflake",
            Self::Sparksql => "sparksql",
            Self::Sqlite => "sqlite",
            Self::Starrocks => "starrocks",
            Self::Teradata => "teradata",
            Self::Trino => "trino",
            Self::Tsql => "tsql",
            Self::Vertica => "vertica",
        }
    }

    /// The dialect a block's tag names, which overrides the `dialect` setting for that block:
    /// `postgresql`, `postgres` and `pgsql` name Postgres, `tsql` and `mssql` T-SQL, and
    /// `mysql`, `mariadb`, `sqlite`, `bigquery`, `snowflake` and `duckdb` their own. `None` for
    /// `sql`, which reads the setting, and for any other tag.
    #[must_use]
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "postgresql" | "postgres" | "pgsql" => Some(Self::Postgres),
            "tsql" | "mssql" => Some(Self::Tsql),
            "mysql" => Some(Self::Mysql),
            "mariadb" => Some(Self::Mariadb),
            "sqlite" => Some(Self::Sqlite),
            "bigquery" => Some(Self::Bigquery),
            "snowflake" => Some(Self::Snowflake),
            "duckdb" => Some(Self::Duckdb),
            _ => None,
        }
    }
}

/// The `[features.code-blocks.sql]` table, exactly as written. Its keys are `formatter`,
/// `linter` and `dialect`, plus one options table per tool; any other key is refused as
/// `config-parse`, and an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSql {
    /// `"sqruff-format"` or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"sqruff-lint"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// One of sqruff's dialect names; unset is `ansi`.
    pub(super) dialect: Option<SqlDialect>,
    /// `[features.code-blocks.sql.sqruff-format]`, when written.
    #[serde(default, rename = "sqruff-format", deserialize_with = "sqruff_format")]
    pub(super) sqruff_format: Option<ToolTable>,
    /// `[features.code-blocks.sql.sqruff-lint]`, when written.
    #[serde(default, rename = "sqruff-lint", deserialize_with = "sqruff_lint")]
    pub(super) sqruff_lint: Option<ToolTable>,
}

/// Reads `formatter` against SQL's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"sqruff-format"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Sql, Step::Format, KNOWN)
}

/// Reads `linter` against SQL's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"sqruff-lint"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Sql, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.sql.sqruff-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn sqruff_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "sqruff-format")
}

/// Reads `[features.code-blocks.sql.sqruff-lint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn sqruff_lint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "sqruff-lint")
}

impl RawSql {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [
                ("sqruff-format", &self.sqruff_format),
                ("sqruff-lint", &self.sqruff_lint),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
