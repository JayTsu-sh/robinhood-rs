//! In-memory predicate evaluation against an [`EntryRow`].
//!
//! Used for fileclass classification at changelog-ingest time and for
//! policy preview (dry-run) without hitting the database.

use rbh_entry_store::model::EntryRow;
use regex::Regex;

use crate::{CmpOp, Field, Predicate, Value};

/// Evaluate `pred` against `entry` in-memory. Returns `true` if the entry
/// matches the predicate.
pub fn matches(pred: &Predicate, entry: &EntryRow) -> bool {
    match pred {
        Predicate::True => true,
        Predicate::False => false,

        Predicate::And { children } => children.iter().all(|c| matches(c, entry)),
        Predicate::Or { children } => children.iter().any(|c| matches(c, entry)),
        Predicate::Not { inner } => !matches(inner, entry),

        Predicate::Cmp { field, cmp, value } => {
            let field_val = extract_field(field, entry);
            compare(&field_val, cmp, value)
        }

        Predicate::NameLike { pattern } => {
            // Match directly on bytes — avoids UTF-8 lossy replacement artifacts
            // on filenames with raw bytes (valid on Lustre).
            like_match(&entry.name, pattern.as_bytes())
        }

        Predicate::InameLike { pattern } => {
            // Case-insensitive: lowercase both sides (ASCII fold; Lustre filenames
            // are almost always ASCII/UTF-8).
            let name_lower = entry.name.to_ascii_lowercase();
            let pat_lower = pattern.to_lowercase();
            like_match(&name_lower, pat_lower.as_bytes())
        }

        Predicate::NameRegex { pattern } => {
            // Compile on each call; patterns are typically short and evaluation
            // is rare in in-memory paths (changelog ingest). If this becomes a
            // hotspot, add a lazily-initialised cache.
            let name = match std::str::from_utf8(&entry.name) {
                Ok(s) => s,
                Err(_) => return false, // non-UTF-8 filenames don't match regex
            };
            Regex::new(pattern).map(|r| r.is_match(name)).unwrap_or(false)
        }

        Predicate::Xattr { key, cmp, value } => {
            let xattr_val = entry.sm_status.get("xattr").and_then(|x| x.get(key));
            match (xattr_val, value) {
                (Some(serde_json::Value::String(s)), Value::Str(rhs)) => {
                    compare(&Value::Str(s.clone()), cmp, &Value::Str(rhs.clone()))
                }
                (Some(serde_json::Value::Number(n)), Value::Num(rhs)) => {
                    let lhs = n.as_i64().unwrap_or(0);
                    compare(&Value::Num(lhs), cmp, &Value::Num(*rhs))
                }
                _ => false,
            }
        }

        Predicate::InPool { pool } => entry.pool_name.as_deref() == Some(pool.as_str()),

        Predicate::HsmStateEq { state } => {
            entry.sm_status.get("hsm_state").and_then(|v| v.as_str()) == Some(state.as_str())
        }

        // OnOst is only meaningful with a DB-side JOIN against stripe_items.
        // EntryRow does not carry the full OST list; treat as false in-memory
        // so the evaluator stays conservative. Push down to SQL for accurate
        // OST filtering.
        Predicate::OnOst { .. } => false,
    }
}

/// Clamp a `u64` to `i64`, saturating at `i64::MAX` instead of wrapping.
fn clamp_u64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Extract a field value from the entry as a [`Value`] for comparison.
fn extract_field(field: &Field, entry: &EntryRow) -> Value {
    match field {
        Field::Size => Value::Num(clamp_u64(entry.size)),
        Field::Blocks => Value::Num(clamp_u64(entry.blocks)),
        Field::Uid => Value::Num(entry.uid as i64),
        Field::Gid => Value::Num(entry.gid as i64),
        Field::Projid => Value::Num(entry.projid as i64),
        Field::Mode => Value::Num(entry.mode as i64),
        Field::Nlink => Value::Num(entry.nlink as i64),
        Field::Mtime => Value::Num(entry.mtime),
        Field::Atime => Value::Num(entry.atime),
        Field::Ctime => Value::Num(entry.ctime),
        Field::Kind => Value::Num(entry.kind as i64),
        Field::StripeCount => Value::Num(entry.stripe_count.unwrap_or(0) as i64),
        Field::StripeSize => Value::Num(entry.stripe_size.unwrap_or(0) as i64),
        Field::LastSeen => Value::Num(entry.last_seen),
        Field::Depth => Value::Num(entry.depth as i64),
    }
}

/// Compare two values using the given operator.
fn compare(lhs: &Value, op: &CmpOp, rhs: &Value) -> bool {
    // Promote both sides to a common numeric representation for comparison.
    match (lhs, rhs) {
        (Value::Num(a), Value::Num(b)) => cmp_ord(*a, op, *b),
        (Value::Str(a), Value::Str(b)) => cmp_ord(a.as_str(), op, b.as_str()),
        _ => false, // cross-type mismatch
    }
}

fn cmp_ord<T: Ord>(a: T, op: &CmpOp, b: T) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
    }
}

/// Maximum LIKE pattern length to prevent exponential backtracking.
const MAX_LIKE_PATTERN_LEN: usize = 256;

fn like_match(text: &[u8], pattern: &[u8]) -> bool {
    if pattern.len() > MAX_LIKE_PATTERN_LEN {
        return false;
    }
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        b'%' => {
            // % matches zero or more chars: try matching rest of pattern against
            // every suffix of text.
            for i in 0..=text.len() {
                if like_match(&text[i..], &pattern[1..]) {
                    return true;
                }
            }
            false
        }
        b'_' => {
            // _ matches exactly one char.
            !text.is_empty() && like_match(&text[1..], &pattern[1..])
        }
        c => !text.is_empty() && text[0] == c && like_match(&text[1..], &pattern[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: string-level LIKE match for readable test assertions.
    fn like_match_str(text: &str, pattern: &str) -> bool {
        like_match(text.as_bytes(), pattern.as_bytes())
    }
    use bytes::Bytes;
    use lustre_api::LuFid;
    use rbh_entry_store::model::EntryKind;

    fn test_entry() -> EntryRow {
        EntryRow {
            fid: LuFid::new(0x200000401, 0x42, 0),
            parent_fid: Some(LuFid::new(0x200000401, 0x01, 0)),
            name: Bytes::from_static(b"report_2026.csv"),
            kind: EntryKind::File,
            size: 5_000_000,
            blocks: 4096,
            uid: 1000,
            gid: 100,
            projid: 0,
            mode: 0o644,
            nlink: 1,
            atime: 1_775_955_820,
            mtime: 1_775_000_000,
            ctime: 1_775_000_000,
            stripe_count: Some(2),
            stripe_size: Some(4_194_304),
            pool_name: Some("ssd".to_string()),
            sm_status: serde_json::json!({}),
            last_seen: 1_775_955_820,
            depth: 2,
        }
    }

    #[test]
    fn matches_true() {
        assert!(matches(&Predicate::True, &test_entry()));
    }

    #[test]
    fn matches_false() {
        assert!(!matches(&Predicate::False, &test_entry()));
    }

    #[test]
    fn matches_size_gt() {
        let pred = Predicate::Cmp {
            field: Field::Size,
            cmp: CmpOp::Gt,
            value: Value::Num(1_000_000),
        };
        assert!(matches(&pred, &test_entry())); // 5M > 1M
    }

    #[test]
    fn matches_uid_eq() {
        let pred = Predicate::Cmp {
            field: Field::Uid,
            cmp: CmpOp::Eq,
            value: Value::Num(1000),
        };
        assert!(matches(&pred, &test_entry()));
    }

    #[test]
    fn matches_uid_ne() {
        let pred = Predicate::Cmp {
            field: Field::Uid,
            cmp: CmpOp::Ne,
            value: Value::Num(9999),
        };
        assert!(matches(&pred, &test_entry()));
    }

    #[test]
    fn matches_and() {
        let pred = Predicate::And {
            children: vec![
                Predicate::Cmp {
                    field: Field::Uid,
                    cmp: CmpOp::Eq,
                    value: Value::Num(1000),
                },
                Predicate::Cmp {
                    field: Field::Size,
                    cmp: CmpOp::Gt,
                    value: Value::Num(100),
                },
            ],
        };
        assert!(matches(&pred, &test_entry()));
    }

    #[test]
    fn matches_or_first_true() {
        let pred = Predicate::Or {
            children: vec![
                Predicate::Cmp {
                    field: Field::Uid,
                    cmp: CmpOp::Eq,
                    value: Value::Num(1000),
                },
                Predicate::False,
            ],
        };
        assert!(matches(&pred, &test_entry()));
    }

    #[test]
    fn matches_not() {
        let pred = Predicate::Not {
            inner: Box::new(Predicate::Cmp {
                field: Field::Size,
                cmp: CmpOp::Lt,
                value: Value::Num(100),
            }),
        };
        assert!(matches(&pred, &test_entry())); // NOT (5M < 100)
    }

    #[test]
    fn matches_name_like() {
        assert!(matches(
            &Predicate::NameLike {
                pattern: "%.csv".to_string()
            },
            &test_entry()
        ));
        assert!(!matches(
            &Predicate::NameLike {
                pattern: "%.txt".to_string()
            },
            &test_entry()
        ));
    }

    #[test]
    fn matches_name_like_underscore() {
        assert!(matches(
            &Predicate::NameLike {
                pattern: "report_2026.cs_".to_string()
            },
            &test_entry()
        ));
    }

    #[test]
    fn matches_in_pool() {
        assert!(matches(
            &Predicate::InPool {
                pool: "ssd".to_string()
            },
            &test_entry()
        ));
        assert!(!matches(
            &Predicate::InPool {
                pool: "archive".to_string()
            },
            &test_entry()
        ));
    }

    #[test]
    fn matches_hsm_state() {
        let mut entry = test_entry();
        entry.sm_status = serde_json::json!({"hsm_state": "archived"});
        assert!(matches(
            &Predicate::HsmStateEq {
                state: "archived".into()
            },
            &entry
        ));
        assert!(!matches(
            &Predicate::HsmStateEq {
                state: "released".into()
            },
            &entry
        ));
        // Missing key -> no match.
        let mut blank = test_entry();
        blank.sm_status = serde_json::json!({});
        assert!(!matches(
            &Predicate::HsmStateEq {
                state: "archived".into()
            },
            &blank
        ));
    }

    #[test]
    fn matches_kind_directory() {
        let mut entry = test_entry();
        entry.kind = EntryKind::Directory;
        let pred = Predicate::Cmp {
            field: Field::Kind,
            cmp: CmpOp::Eq,
            value: Value::Num(1), // Directory
        };
        assert!(matches(&pred, &entry));
    }

    #[test]
    fn matches_iname_like() {
        let entry = test_entry(); // name = "report_2026.csv"
        assert!(matches(
            &Predicate::InameLike {
                pattern: "REPORT_%.CSV".to_string()
            },
            &entry
        ));
        assert!(matches(
            &Predicate::InameLike {
                pattern: "%.CSV".to_string()
            },
            &entry
        ));
        assert!(!matches(
            &Predicate::InameLike {
                pattern: "%.TXT".to_string()
            },
            &entry
        ));
    }

    #[test]
    fn matches_name_regex() {
        let entry = test_entry(); // name = "report_2026.csv"
        assert!(matches(
            &Predicate::NameRegex {
                pattern: r"^report_\d+\.csv$".to_string()
            },
            &entry
        ));
        assert!(!matches(
            &Predicate::NameRegex {
                pattern: r"^backup_".to_string()
            },
            &entry
        ));
        // Invalid regex → false (no panic)
        assert!(!matches(
            &Predicate::NameRegex {
                pattern: "[invalid".to_string()
            },
            &entry
        ));
    }

    #[test]
    fn matches_xattr_string() {
        let mut entry = test_entry();
        entry.sm_status = serde_json::json!({"xattr": {"user.tier": "hot"}});
        assert!(matches(
            &Predicate::Xattr {
                key: "user.tier".to_string(),
                cmp: CmpOp::Eq,
                value: Value::Str("hot".to_string()),
            },
            &entry
        ));
        assert!(!matches(
            &Predicate::Xattr {
                key: "user.tier".to_string(),
                cmp: CmpOp::Eq,
                value: Value::Str("cold".to_string()),
            },
            &entry
        ));
        // Missing key → false
        assert!(!matches(
            &Predicate::Xattr {
                key: "user.missing".to_string(),
                cmp: CmpOp::Eq,
                value: Value::Str("hot".to_string()),
            },
            &entry
        ));
    }

    #[test]
    fn matches_depth_field() {
        let mut entry = test_entry();
        entry.depth = 3;
        let deep = Predicate::Cmp {
            field: Field::Depth,
            cmp: CmpOp::Gt,
            value: Value::Num(2),
        };
        let shallow = Predicate::Cmp {
            field: Field::Depth,
            cmp: CmpOp::Le,
            value: Value::Num(1),
        };
        assert!(matches(&deep, &entry));
        assert!(!matches(&shallow, &entry));
    }

    #[test]
    fn like_match_edge_cases() {
        assert!(like_match_str("", ""));
        assert!(like_match_str("", "%"));
        assert!(!like_match_str("", "_"));
        assert!(like_match_str("abc", "abc"));
        assert!(like_match_str("abc", "%"));
        assert!(like_match_str("abc", "a%"));
        assert!(like_match_str("abc", "%c"));
        assert!(like_match_str("abc", "a_c"));
        assert!(!like_match_str("abc", "a_d"));
        assert!(like_match_str("abc", "%%"));
        assert!(like_match_str("a", "_"));
    }
}
