//! `rbh find` — query the catalog with `find(1)`-style predicates.
//!
//! Options map directly to [`rbh_predicate::Predicate`] nodes and are sent
//! as a `POST /api/entries/query` body. Output formatting happens
//! client-side so the daemon can stay format-agnostic.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clap::Args;
use rbh_predicate::{CmpOp, Field, Predicate, SortKey, Value};

/// CLI arguments for `rbh find`. Flattened into the main Cli via clap.
#[derive(Args, Debug, Default, Clone)]
pub struct FindArgs {
    /// UID (numeric).
    #[arg(long)]
    pub user: Option<u32>,
    /// GID (numeric).
    #[arg(long)]
    pub group: Option<u32>,
    /// Project id.
    #[arg(long)]
    pub projid: Option<u32>,
    /// Lustre pool name.
    #[arg(long)]
    pub pool: Option<String>,
    /// Entry type: f (file) | d (dir) | l (symlink) | b | c | p | s.
    #[arg(long)]
    pub r#type: Option<char>,
    /// Name glob (SQL LIKE, `%` = any, `_` = single char).
    #[arg(long)]
    pub name: Option<String>,
    /// Size filter: `+10M`, `-1K`, `5G` (no prefix = exact).
    #[arg(long)]
    pub size: Option<String>,
    /// Modified-time filter: `+7d`, `-1h`, `10m`.
    #[arg(long)]
    pub mtime: Option<String>,
    /// Access-time filter (same grammar as --mtime).
    #[arg(long)]
    pub atime: Option<String>,
    /// Change-time filter (same grammar as --mtime).
    #[arg(long)]
    pub ctime: Option<String>,

    /// Max rows to fetch per page.
    #[arg(long, default_value = "1000")]
    pub limit: u64,
    /// Skip this many rows.
    #[arg(long, default_value = "0")]
    pub offset: u64,
    /// Sort key: one of size, mtime, atime, ctime, uid, gid.
    #[arg(long)]
    pub sort: Option<String>,
    /// Sort ascending when true (default), descending when false.
    #[arg(long, default_value = "true")]
    pub asc: bool,

    /// Print as JSON instead of one-line summary.
    #[arg(long)]
    pub json: bool,
}

/// Build the `POST /api/entries/query` request body from `FindArgs`.
///
/// Returns `(Predicate, Vec<SortKey>)` so tests can inspect the result
/// without hitting HTTP.
pub fn build_query(args: &FindArgs, now: i64) -> Result<(Predicate, Vec<SortKey>)> {
    let mut preds = Vec::<Predicate>::new();

    if let Some(uid) = args.user {
        preds.push(eq_num(Field::Uid, uid as i64));
    }
    if let Some(gid) = args.group {
        preds.push(eq_num(Field::Gid, gid as i64));
    }
    if let Some(pid) = args.projid {
        preds.push(eq_num(Field::Projid, pid as i64));
    }
    if let Some(pool) = &args.pool {
        preds.push(Predicate::InPool { pool: pool.clone() });
    }
    if let Some(t) = args.r#type {
        preds.push(eq_num(Field::Kind, type_char_to_kind(t)? as i64));
    }
    if let Some(n) = &args.name {
        // Respect the SQL LIKE contract — caller owns escaping if they
        // really mean a literal `%` or `_`.
        preds.push(Predicate::NameLike { pattern: n.clone() });
    }
    if let Some(s) = &args.size {
        preds.push(parse_size_filter(s)?);
    }
    if let Some(t) = &args.mtime {
        preds.push(parse_time_filter(Field::Mtime, t, now)?);
    }
    if let Some(t) = &args.atime {
        preds.push(parse_time_filter(Field::Atime, t, now)?);
    }
    if let Some(t) = &args.ctime {
        preds.push(parse_time_filter(Field::Ctime, t, now)?);
    }

    let predicate = match preds.len() {
        0 => Predicate::True,
        1 => preds.into_iter().next().unwrap(),
        _ => Predicate::And { children: preds },
    };

    let order_by = if let Some(name) = &args.sort {
        let field = parse_sort_field(name)?;
        vec![SortKey {
            field,
            dir: if args.asc {
                rbh_predicate::OrderDir::Asc
            } else {
                rbh_predicate::OrderDir::Desc
            },
        }]
    } else {
        Vec::new()
    };

    Ok((predicate, order_by))
}

fn eq_num(field: Field, n: i64) -> Predicate {
    Predicate::Cmp { field, cmp: CmpOp::Eq, value: Value::Num(n) }
}

fn type_char_to_kind(c: char) -> Result<u8> {
    match c {
        'f' => Ok(0),
        'd' => Ok(1),
        'l' => Ok(2),
        'c' => Ok(3),
        'b' => Ok(4),
        'p' => Ok(5),
        's' => Ok(6),
        _ => Err(anyhow!(
            "invalid --type {c:?}: expected one of f d l b c p s"
        )),
    }
}

/// Parse a find-style size argument like `+10M`, `-1K`, `5G`.
pub fn parse_size_filter(s: &str) -> Result<Predicate> {
    let (cmp, body) = split_sign(s);
    let (num, unit) = split_number_unit(body)?;
    let bytes = num
        .checked_mul(size_unit_multiplier(unit)?)
        .ok_or_else(|| anyhow!("size overflow: {s}"))?;
    Ok(Predicate::Cmp {
        field: Field::Size,
        cmp,
        value: Value::Num(bytes),
    })
}

/// Parse a find-style time argument like `+7d`, `-1h`. Positive prefix
/// means "older than" (value BEFORE now-N) — same semantics as find(1).
pub fn parse_time_filter(field: Field, s: &str, now: i64) -> Result<Predicate> {
    let (sign_cmp, body) = split_sign(s);
    let (num, unit) = split_number_unit(body)?;
    let secs = num
        .checked_mul(time_unit_seconds(unit)?)
        .ok_or_else(|| anyhow!("time overflow: {s}"))?;
    let threshold = now - secs;

    // find(1) time semantics (inverted vs. size because newer = larger ts):
    //   +N  → older than N ago     → field <  now-N  → Lt
    //   -N  → younger than N ago   → field >  now-N  → Gt
    //   N   → exactly at N ago     → field <= now-N  (approximation)
    let cmp = match sign_cmp {
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Lt => CmpOp::Gt,
        _ => CmpOp::Le,
    };

    Ok(Predicate::Cmp {
        field,
        cmp,
        value: Value::Num(threshold),
    })
}

fn parse_sort_field(name: &str) -> Result<Field> {
    Ok(match name {
        "size" => Field::Size,
        "mtime" => Field::Mtime,
        "atime" => Field::Atime,
        "ctime" => Field::Ctime,
        "uid" => Field::Uid,
        "gid" => Field::Gid,
        "nlink" => Field::Nlink,
        other => return Err(anyhow!("unknown --sort field: {other}")),
    })
}

fn split_sign(s: &str) -> (CmpOp, &str) {
    // `+N` → Gt (greater/older for size, or for raw numeric cmp).
    // `-N` → Lt (smaller/younger).
    // Note: for time filters, the caller inverts this to preserve find(1)
    // semantics (older-than uses Lt on the timestamp).
    if let Some(rest) = s.strip_prefix('+') {
        (CmpOp::Gt, rest)
    } else if let Some(rest) = s.strip_prefix('-') {
        (CmpOp::Lt, rest)
    } else {
        (CmpOp::Eq, s)
    }
}

fn split_number_unit(s: &str) -> Result<(i64, char)> {
    if s.is_empty() {
        return Err(anyhow!("empty numeric argument"));
    }
    let (num_part, unit) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&s[..s.len() - c.len_utf8()], c),
        _ => (s, '_'),
    };
    let num: i64 = num_part
        .parse()
        .with_context(|| format!("not an integer: {num_part:?}"))?;
    Ok((num, unit))
}

fn size_unit_multiplier(c: char) -> Result<i64> {
    Ok(match c.to_ascii_uppercase() {
        '_' | 'B' => 1,
        'K' => 1024,
        'M' => 1024 * 1024,
        'G' => 1024 * 1024 * 1024,
        'T' => 1024i64 * 1024 * 1024 * 1024,
        other => return Err(anyhow!("unknown size unit: {other}")),
    })
}

fn time_unit_seconds(c: char) -> Result<i64> {
    Ok(match c {
        '_' | 'd' => 86_400,
        'h' => 3_600,
        'm' => 60,
        's' => 1,
        'y' => 365 * 86_400,
        other => return Err(anyhow!("unknown time unit: {other}")),
    })
}

/// Current unix epoch — isolated so tests can pin `now`.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_args_yields_true() {
        let (p, keys) = build_query(&FindArgs::default(), 0).unwrap();
        assert_eq!(p, Predicate::True);
        assert!(keys.is_empty());
    }

    #[test]
    fn user_and_size_combine_with_and() {
        let args = FindArgs {
            user: Some(1000),
            size: Some("+1M".to_string()),
            ..Default::default()
        };
        let (p, _) = build_query(&args, 0).unwrap();
        match p {
            Predicate::And { children } => {
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected And"),
        }
    }

    #[test]
    fn size_plus_uses_gt() {
        let p = parse_size_filter("+1K").unwrap();
        assert_eq!(
            p,
            Predicate::Cmp {
                field: Field::Size,
                cmp: CmpOp::Gt,
                value: Value::Num(1024)
            }
        );
    }

    #[test]
    fn size_minus_uses_lt() {
        let p = parse_size_filter("-1M").unwrap();
        assert_eq!(
            p,
            Predicate::Cmp {
                field: Field::Size,
                cmp: CmpOp::Lt,
                value: Value::Num(1_048_576)
            }
        );
    }

    #[test]
    fn size_bare_bytes() {
        let p = parse_size_filter("42").unwrap();
        assert_eq!(
            p,
            Predicate::Cmp {
                field: Field::Size,
                cmp: CmpOp::Eq,
                value: Value::Num(42)
            }
        );
    }

    #[test]
    fn mtime_plus_older_than() {
        // find: -mtime +7  → older than 7 days → mtime < now - 7d
        let now = 1_700_000_000;
        let p = parse_time_filter(Field::Mtime, "+7d", now).unwrap();
        assert_eq!(
            p,
            Predicate::Cmp {
                field: Field::Mtime,
                cmp: CmpOp::Lt,
                value: Value::Num(now - 7 * 86_400)
            }
        );
    }

    #[test]
    fn mtime_minus_younger_than() {
        // find: -mtime -1  → newer than 1 day → mtime > now - 1d
        let now = 1_700_000_000;
        let p = parse_time_filter(Field::Mtime, "-1d", now).unwrap();
        assert_eq!(
            p,
            Predicate::Cmp {
                field: Field::Mtime,
                cmp: CmpOp::Gt,
                value: Value::Num(now - 86_400)
            }
        );
    }

    #[test]
    fn type_char_decodes_to_kind() {
        assert_eq!(type_char_to_kind('f').unwrap(), 0);
        assert_eq!(type_char_to_kind('d').unwrap(), 1);
        assert_eq!(type_char_to_kind('l').unwrap(), 2);
        assert!(type_char_to_kind('x').is_err());
    }

    #[test]
    fn sort_field_rejects_unknown() {
        assert!(parse_sort_field("bogus").is_err());
        assert_eq!(parse_sort_field("mtime").unwrap(), Field::Mtime);
    }

    #[test]
    fn unknown_size_unit_rejected() {
        assert!(parse_size_filter("10Z").is_err());
    }

    #[test]
    fn sort_flag_produces_single_key() {
        let args = FindArgs {
            sort: Some("size".into()),
            asc: false,
            ..Default::default()
        };
        let (_, keys) = build_query(&args, 0).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].field, Field::Size);
        assert_eq!(keys[0].dir, rbh_predicate::OrderDir::Desc);
    }
}
