//! Policy predicate tree — replaces robinhood-C's text DSL (`rbh_boolexpr`).
//!
//! A [`Predicate`] is a JSON-serializable boolean expression tree that can be:
//! * Translated to a parameterized SQL WHERE clause via [`Predicate::to_sql`]
//!   (pushed down to MariaDB for efficient candidate selection).
//! * Evaluated in-memory via [`Predicate::matches`] against an [`EntryRow`]
//!   (used for fileclass classification at changelog-ingest time).
//!
//! See `.claude/memory/rust_design_style.md` rule 14: typed predicate fields,
//! not strings.

mod eval;
mod sql;

use serde::{Deserialize, Serialize};

pub use eval::matches;
pub use sql::{OrderDir, SortKey, SqlParam, to_sql};

/// A boolean predicate over entry attributes.
///
/// Designed for JSON round-tripping via `#[serde(tag = "op")]`. API clients
/// POST these as part of policy bodies; the server validates at parse time
/// (invalid field names are rejected by serde, not at SQL execution time).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Predicate {
    /// All children must match.
    And { children: Vec<Predicate> },
    /// At least one child must match.
    Or { children: Vec<Predicate> },
    /// Negate the inner predicate.
    Not { inner: Box<Predicate> },
    /// Compare a field against a value.
    Cmp { field: Field, cmp: CmpOp, value: Value },
    /// Case-sensitive SQL LIKE on the entry name. Pattern uses `%` and `_` wildcards.
    /// Callers must escape literal `%` / `_` in user-supplied input.
    NameLike { pattern: String },
    /// Case-insensitive name match. Equivalent to `LOWER(name) LIKE LOWER(pattern)`.
    /// Use for shell-style glob patterns where case should be ignored.
    InameLike { pattern: String },
    /// POSIX extended-regex match on the entry name (`name REGEXP pattern` in MariaDB).
    /// In-memory evaluation uses the `regex` crate (same ERE syntax).
    NameRegex { pattern: String },
    /// Match an extended attribute stored under `sm_status.xattr.<key>`.
    /// Assumes `sm_status` JSON has shape `{"xattr": {"user.tier": "hot", …}}`.
    Xattr { key: String, cmp: CmpOp, value: Value },
    /// Match entries whose pool_name equals the given string.
    InPool { pool: String },
    /// Match entries with a stripe on any of the given OST indices.
    /// Generated SQL is `EXISTS (SELECT 1 FROM stripe_items s WHERE
    /// s.fid = entries.fid AND s.ost_index IN (?, ?, …))`.
    OnOst { osts: Vec<u32> },
    /// Match entries whose `sm_status.hsm_state` JSON field equals the
    /// given string (e.g. `"archived"`, `"released"`, `"none"`). Uses
    /// MySQL's `JSON_UNQUOTE(JSON_EXTRACT(...))` so the comparison is
    /// insensitive to JSON escape form.
    HsmStateEq { state: String },
    /// Match entries where ALL tag key-value pairs are present in
    /// `sm_status.xattr`. Shorthand for `AND(Xattr(k1=v1), Xattr(k2=v2), …)`.
    /// An empty `match_tags` map is equivalent to `True`.
    Tags {
        match_tags: std::collections::HashMap<String, String>,
    },
    /// Always true — useful as default rule condition.
    True,
    /// Always false.
    False,
}

/// Typed field identifiers — one variant per filterable column in `entries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Size,
    Blocks,
    Uid,
    Gid,
    Projid,
    Mode,
    Nlink,
    Mtime,
    Atime,
    Ctime,
    Kind,
    StripeCount,
    StripeSize,
    LastSeen,
    /// Directory depth from filesystem root (0 = root, 1 = first level, …).
    /// Populated by the initial fs-scan; changelog-ingested entries default to 0.
    Depth,
}

impl Field {
    /// SQL column name in the `entries` table.
    pub fn column_name(&self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Blocks => "blocks",
            Self::Uid => "uid",
            Self::Gid => "gid",
            Self::Projid => "projid",
            Self::Mode => "mode",
            Self::Nlink => "nlink",
            Self::Mtime => "mtime",
            Self::Atime => "atime",
            Self::Ctime => "ctime",
            Self::Kind => "kind",
            Self::StripeCount => "stripe_count",
            Self::StripeSize => "stripe_size",
            Self::LastSeen => "last_seen",
            Self::Depth => "depth",
        }
    }
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl CmpOp {
    /// SQL operator string.
    pub fn sql(&self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
        }
    }
}

/// Typed comparison value. Numbers are stored as `i64` uniformly —
/// unsigned values are clamped at extraction time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    /// Numeric value (both signed and unsigned map here via JSON).
    Num(i64),
    /// String value (for pool_name, etc.).
    Str(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_cmp() {
        let pred = Predicate::Cmp {
            field: Field::Size,
            cmp: CmpOp::Gt,
            value: Value::Num(1_000_000),
        };
        let json = serde_json::to_string(&pred).unwrap();
        assert!(json.contains(r#""op":"cmp""#));
        assert!(json.contains(r#""field":"size""#));
        let back: Predicate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pred);
    }

    #[test]
    fn serde_roundtrip_and() {
        let pred = Predicate::And {
            children: vec![
                Predicate::Cmp {
                    field: Field::Uid,
                    cmp: CmpOp::Eq,
                    value: Value::Num(1000),
                },
                Predicate::Cmp {
                    field: Field::Mtime,
                    cmp: CmpOp::Lt,
                    value: Value::Num(1_775_955_820),
                },
            ],
        };
        let json = serde_json::to_string(&pred).unwrap();
        let back: Predicate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pred);
    }

    #[test]
    fn serde_roundtrip_not() {
        let pred = Predicate::Not {
            inner: Box::new(Predicate::NameLike {
                pattern: "%.tmp".to_string(),
            }),
        };
        let json = serde_json::to_string(&pred).unwrap();
        let back: Predicate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pred);
    }

    #[test]
    fn serde_true_false() {
        let t: Predicate = serde_json::from_str(r#"{"op":"true"}"#).unwrap();
        assert_eq!(t, Predicate::True);
        let f: Predicate = serde_json::from_str(r#"{"op":"false"}"#).unwrap();
        assert_eq!(f, Predicate::False);
    }
}
