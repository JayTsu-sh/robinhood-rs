//! `Predicate::to_sql` — generate a parameterized SQL WHERE fragment.
//!
//! Parameters are collected as [`SqlParam`] values and appended to a `Vec`;
//! the caller binds them positionally via sqlx `query.bind(...)`.

use serde::{Deserialize, Serialize};

use crate::{Field, Predicate, Value};

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderDir {
    Asc,
    Desc,
}

impl OrderDir {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// A sort key — pair of validated field + direction. Never constructed from
/// raw user-provided column names; all columns come through [`Field`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    pub field: Field,
    pub dir: OrderDir,
}

impl SortKey {
    /// Render a safe `"<col> ASC|DESC"` fragment for SQL composition.
    pub fn to_sql_fragment(&self) -> String {
        format!("{} {}", self.field.column_name(), self.dir.as_sql())
    }

    /// Join a slice of keys into a comma-separated fragment.
    pub fn list_to_sql(keys: &[SortKey]) -> String {
        keys.iter().map(SortKey::to_sql_fragment).collect::<Vec<_>>().join(", ")
    }
}

/// A parameter value to bind in the generated SQL.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Num(i64),
    Str(String),
}

/// Generate a parameterized WHERE clause fragment from a predicate.
///
/// Returns `(sql_fragment, params)` where `sql_fragment` contains `?`
/// placeholders and `params` contains the values to bind in order.
///
/// # Examples
///
/// ```
/// use rbh_predicate::*;
/// let pred = Predicate::Cmp { field: Field::Size, cmp: CmpOp::Gt, value: Value::Num(1000) };
/// let (sql, params) = to_sql(&pred);
/// assert_eq!(sql, "size > ?");
/// assert_eq!(params, vec![SqlParam::Num(1000)]);
/// ```
pub fn to_sql(pred: &Predicate) -> (String, Vec<SqlParam>) {
    let mut params = Vec::new();
    let sql = build(pred, &mut params);
    (sql, params)
}

fn build(pred: &Predicate, params: &mut Vec<SqlParam>) -> String {
    match pred {
        Predicate::True => "1=1".to_string(),
        Predicate::False => "1=0".to_string(),

        Predicate::Cmp { field, cmp, value } => {
            params.push(value_to_param(value));
            format!("{} {} ?", field.column_name(), cmp.sql())
        }

        Predicate::NameLike { pattern } => {
            params.push(SqlParam::Str(pattern.clone()));
            "name LIKE ?".to_string()
        }

        Predicate::InameLike { pattern } => {
            params.push(SqlParam::Str(pattern.clone()));
            "LOWER(name) LIKE LOWER(?)".to_string()
        }

        Predicate::NameRegex { pattern } => {
            params.push(SqlParam::Str(pattern.clone()));
            "name REGEXP ?".to_string()
        }

        Predicate::Xattr { key, cmp, value } => {
            let json_path = format!("$.xattr.{key}");
            params.push(SqlParam::Str(json_path.clone()));
            params.push(value_to_param(value));
            format!("JSON_UNQUOTE(JSON_EXTRACT(sm_status, ?)) {} ?", cmp.sql())
        }

        Predicate::InPool { pool } => {
            params.push(SqlParam::Str(pool.clone()));
            "pool_name = ?".to_string()
        }

        Predicate::HsmStateEq { state } => {
            params.push(SqlParam::Str(state.clone()));
            "JSON_UNQUOTE(JSON_EXTRACT(sm_status, '$.hsm_state')) = ?".to_string()
        }

        Predicate::OnOst { osts } => {
            if osts.is_empty() {
                return "1=0".to_string();
            }
            let placeholders = (0..osts.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
            for idx in osts {
                params.push(SqlParam::Num(*idx as i64));
            }
            format!(
                "EXISTS (SELECT 1 FROM scoped_stripe_items s \
                 WHERE s.filesystem_id = entries.filesystem_id \
                 AND s.object_kind = entries.object_kind \
                 AND s.object_id = entries.object_id \
                 AND s.ost_index IN ({placeholders}))"
            )
        }

        Predicate::And { children } => {
            if children.is_empty() {
                return "1=1".to_string(); // empty AND = true
            }
            let parts: Vec<String> = children.iter().map(|c| build(c, params)).collect();
            format!("({})", parts.join(" AND "))
        }

        Predicate::Or { children } => {
            if children.is_empty() {
                return "1=0".to_string(); // empty OR = false
            }
            let parts: Vec<String> = children.iter().map(|c| build(c, params)).collect();
            format!("({})", parts.join(" OR "))
        }

        Predicate::Not { inner } => {
            let inner_sql = build(inner, params);
            format!("NOT ({})", inner_sql)
        }

        Predicate::Tags { match_tags } => {
            if match_tags.is_empty() {
                return "1=1".to_string();
            }
            // Expand to AND(Xattr(k1=v1), Xattr(k2=v2), …) — sort keys for determinism.
            let mut keys: Vec<&String> = match_tags.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    let json_path = format!("$.xattr.{k}");
                    params.push(SqlParam::Str(json_path));
                    params.push(SqlParam::Str(match_tags[*k].clone()));
                    "JSON_UNQUOTE(JSON_EXTRACT(sm_status, ?)) = ?".to_string()
                })
                .collect();
            if parts.len() == 1 {
                parts.into_iter().next().unwrap()
            } else {
                format!("({})", parts.join(" AND "))
            }
        }
    }
}

fn value_to_param(v: &Value) -> SqlParam {
    match v {
        Value::Num(n) => SqlParam::Num(*n),
        Value::Str(s) => SqlParam::Str(s.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn simple_cmp() {
        let pred = Predicate::Cmp {
            field: Field::Size,
            cmp: CmpOp::Gt,
            value: Value::Num(1_000_000),
        };
        let (sql, params) = to_sql(&pred);
        assert_eq!(sql, "size > ?");
        assert_eq!(params, vec![SqlParam::Num(1_000_000)]);
    }

    #[test]
    fn and_two_conditions() {
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
        let (sql, params) = to_sql(&pred);
        assert_eq!(sql, "(uid = ? AND mtime < ?)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn or_with_name_like() {
        let pred = Predicate::Or {
            children: vec![
                Predicate::NameLike {
                    pattern: "%.tmp".to_string(),
                },
                Predicate::NameLike {
                    pattern: "%.log".to_string(),
                },
            ],
        };
        let (sql, params) = to_sql(&pred);
        assert_eq!(sql, "(name LIKE ? OR name LIKE ?)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn not_wraps_inner() {
        let pred = Predicate::Not {
            inner: Box::new(Predicate::InPool {
                pool: "flash".to_string(),
            }),
        };
        let (sql, params) = to_sql(&pred);
        assert_eq!(sql, "NOT (pool_name = ?)");
        assert_eq!(params, vec![SqlParam::Str("flash".to_string())]);
    }

    #[test]
    fn true_false() {
        assert_eq!(to_sql(&Predicate::True).0, "1=1");
        assert_eq!(to_sql(&Predicate::False).0, "1=0");
    }

    #[test]
    fn empty_and_is_true() {
        let (sql, _) = to_sql(&Predicate::And { children: vec![] });
        assert_eq!(sql, "1=1");
    }

    #[test]
    fn empty_or_is_false() {
        let (sql, _) = to_sql(&Predicate::Or { children: vec![] });
        assert_eq!(sql, "1=0");
    }

    #[test]
    fn nested_and_or() {
        let pred = Predicate::And {
            children: vec![
                Predicate::Cmp {
                    field: Field::Kind,
                    cmp: CmpOp::Eq,
                    value: Value::Num(0), // file
                },
                Predicate::Or {
                    children: vec![
                        Predicate::Cmp {
                            field: Field::Size,
                            cmp: CmpOp::Gt,
                            value: Value::Num(1_000_000_000),
                        },
                        Predicate::Cmp {
                            field: Field::Mtime,
                            cmp: CmpOp::Lt,
                            value: Value::Num(1_700_000_000),
                        },
                    ],
                },
            ],
        };
        let (sql, params) = to_sql(&pred);
        assert_eq!(sql, "(kind = ? AND (size > ? OR mtime < ?))");
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn param_ordering_matches_placeholders() {
        let pred = Predicate::And {
            children: vec![
                Predicate::Cmp {
                    field: Field::Uid,
                    cmp: CmpOp::Eq,
                    value: Value::Num(1000),
                },
                Predicate::Cmp {
                    field: Field::Gid,
                    cmp: CmpOp::Eq,
                    value: Value::Num(100),
                },
                Predicate::Cmp {
                    field: Field::Size,
                    cmp: CmpOp::Ge,
                    value: Value::Num(0),
                },
            ],
        };
        let (_, params) = to_sql(&pred);
        assert_eq!(params, vec![SqlParam::Num(1000), SqlParam::Num(100), SqlParam::Num(0)]);
    }

    #[test]
    fn on_ost_single_placeholder() {
        let pred = Predicate::OnOst { osts: vec![3] };
        let (sql, params) = to_sql(&pred);
        assert!(
            sql.contains("EXISTS (SELECT 1 FROM scoped_stripe_items s"),
            "missing EXISTS: {sql}"
        );
        assert!(sql.contains("IN (?)"), "unexpected SQL: {sql}");
        assert_eq!(params, vec![SqlParam::Num(3)]);
    }

    #[test]
    fn on_ost_multi_placeholders() {
        let pred = Predicate::OnOst { osts: vec![1, 4, 7] };
        let (sql, params) = to_sql(&pred);
        assert!(sql.contains("IN (?, ?, ?)"));
        assert_eq!(params, vec![SqlParam::Num(1), SqlParam::Num(4), SqlParam::Num(7)]);
    }

    #[test]
    fn on_ost_empty_is_false() {
        let pred = Predicate::OnOst { osts: vec![] };
        assert_eq!(to_sql(&pred).0, "1=0");
    }

    #[test]
    fn iname_like_lowercases_both_sides() {
        let p = Predicate::InameLike {
            pattern: "%.CSV".to_string(),
        };
        let (sql, params) = to_sql(&p);
        assert_eq!(sql, "LOWER(name) LIKE LOWER(?)");
        assert_eq!(params, vec![SqlParam::Str("%.CSV".to_string())]);
    }

    #[test]
    fn name_regex_generates_regexp() {
        let p = Predicate::NameRegex {
            pattern: r"^report_\d+".to_string(),
        };
        let (sql, params) = to_sql(&p);
        assert_eq!(sql, "name REGEXP ?");
        assert_eq!(params, vec![SqlParam::Str(r"^report_\d+".to_string())]);
    }

    #[test]
    fn xattr_generates_json_extract_with_cmp() {
        let p = Predicate::Xattr {
            key: "user.tier".to_string(),
            cmp: CmpOp::Eq,
            value: Value::Str("hot".to_string()),
        };
        let (sql, params) = to_sql(&p);
        assert!(sql.contains("JSON_UNQUOTE(JSON_EXTRACT(sm_status, ?))"), "sql={sql}");
        assert!(sql.contains("= ?"), "sql={sql}");
        assert_eq!(params[0], SqlParam::Str("$.xattr.user.tier".to_string()));
        assert_eq!(params[1], SqlParam::Str("hot".to_string()));
    }

    #[test]
    fn depth_field_in_cmp() {
        let p = Predicate::Cmp {
            field: Field::Depth,
            cmp: CmpOp::Le,
            value: Value::Num(3),
        };
        let (sql, params) = to_sql(&p);
        assert_eq!(sql, "depth <= ?");
        assert_eq!(params, vec![SqlParam::Num(3)]);
    }

    #[test]
    fn hsm_state_generates_json_extract() {
        let p = Predicate::HsmStateEq {
            state: "archived".into(),
        };
        let (sql, params) = to_sql(&p);
        assert!(sql.contains("JSON_UNQUOTE"));
        assert!(sql.contains("$.hsm_state"));
        assert_eq!(params, vec![SqlParam::Str("archived".into())]);
    }
}
