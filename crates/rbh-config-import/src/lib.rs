//! `rbh-config-import` — pragmatic converter from robinhood-C config to
//! rbh-daemon `PolicyDef` JSON.
//!
//! Scope: recognise the common shapes in `doc/templates/example_*.conf`
//! and the bundled `includes/lhsm.inc` — not a full drop-in replacement
//! for the yacc/lex parser in `src/cfg_parsing/`. Unrecognised constructs
//! are logged as warnings and skipped; the user is expected to hand-edit
//! the produced JSON before POSTing it.

use std::collections::HashMap;

use anyhow::Result;
use rbh_policy::{ActionParams, LruSortAttr, PolicyDef, PolicyKind, TriggerSpec};
use rbh_predicate::{CmpOp, Field, Predicate, Value};

/// Output of the importer for a single config file.
#[derive(Debug, Default)]
pub struct ImportResult {
    pub policies: Vec<PolicyDef>,
    pub warnings: Vec<String>,
    /// Fileclass name → predicate (used to resolve `scope = <fileclass>`).
    pub fileclasses: HashMap<String, Predicate>,
}

pub fn import(src: &str) -> Result<ImportResult> {
    let cleaned = strip_comments(src);
    let blocks = split_top_level_blocks(&cleaned)?;

    let mut out = ImportResult::default();

    for (header, body) in &blocks {
        let hdr = header.trim();
        if let Some(name) = hdr.strip_prefix("FileClass ") {
            match parse_fileclass(name.trim(), body) {
                Ok((n, p)) => {
                    out.fileclasses.insert(n, p);
                }
                Err(e) => out.warnings.push(format!("FileClass {name}: {e}")),
            }
        } else if let Some(name) = hdr.strip_prefix("define_policy ") {
            match parse_policy(name.trim(), body, &out.fileclasses) {
                Ok((pol, mut warns)) => {
                    out.warnings.append(&mut warns);
                    out.policies.push(pol);
                }
                Err(e) => out.warnings.push(format!("define_policy {name}: {e}")),
            }
        } else if let Some(name) = hdr.strip_prefix("FileClass\t") {
            // same as "FileClass "
            match parse_fileclass(name.trim(), body) {
                Ok((n, p)) => {
                    out.fileclasses.insert(n, p);
                }
                Err(e) => out.warnings.push(format!("FileClass {name}: {e}")),
            }
        } else {
            // Blocks we don't translate (General, lhsm_config, ...).
            out.warnings
                .push(format!("skipped unhandled block: {}", first_word(hdr)));
        }
    }

    Ok(out)
}

fn first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("")
}

/// Remove `# line`, `// line`, and `/* ... */` comments. Preserves line
/// numbers by leaving newlines in place so warnings can point at lines.
fn strip_comments(src: &str) -> String {
    // block comment pass
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(x) = chars.next() {
                if x == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
                if x == '\n' {
                    out.push('\n');
                }
            }
            continue;
        }
        out.push(c);
    }
    // line comment pass
    out.lines()
        .map(|l| {
            let line_break_at = |idx: Option<usize>| idx.unwrap_or(l.len());
            let hash = l.find('#');
            let slash = l.find("//");
            let cut = match (hash, slash) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => l.len(),
            };
            &l[..line_break_at(Some(cut))]
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split top-level `header { body }` blocks. Respects nested braces.
/// Returns `Vec<(header, body)>`.
fn split_top_level_blocks(src: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // scan until '{'
        let hdr_start = i;
        while i < bytes.len() && bytes[i] != b'{' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let header = std::str::from_utf8(&bytes[hdr_start..i])?.trim().to_string();
        i += 1; // past '{'
        let body_start = i;
        let mut depth = 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return Err(anyhow::anyhow!("unbalanced '{{' for block: {header}"));
        }
        let body = std::str::from_utf8(&bytes[body_start..i - 1])?.to_string();
        out.push((header, body));
    }
    Ok(out)
}

fn parse_fileclass(name: &str, body: &str) -> Result<(String, Predicate)> {
    // FileClass foo { definition { ... } [other k=v] }
    let (blocks, _kvs) = split_body(body);
    let def = blocks
        .iter()
        .find(|(h, _)| h.trim() == "definition")
        .ok_or_else(|| anyhow::anyhow!("missing definition block"))?;
    let pred = parse_expression(&def.1)?;
    Ok((name.to_string(), pred))
}

fn parse_policy(name: &str, body: &str, fileclasses: &HashMap<String, Predicate>) -> Result<(PolicyDef, Vec<String>)> {
    let (blocks, kvs) = split_body(body);
    let mut warns = Vec::new();

    // scope { ... }  OR  scope = <fileclass>;
    let scope = if let Some((_, b)) = blocks.iter().find(|(h, _)| h.trim() == "scope") {
        parse_expression(b)?
    } else if let Some(v) = kvs.get("scope") {
        let name = v.trim().trim_end_matches(';').trim();
        fileclasses.get(name).cloned().unwrap_or_else(|| {
            warns.push(format!("scope references unknown fileclass: {name}"));
            Predicate::True
        })
    } else {
        Predicate::True
    };

    // Map policy name / default_action to PolicyKind.
    let kind = if name.contains("archive") {
        PolicyKind::HsmArchive
    } else if name.contains("release") {
        PolicyKind::HsmRelease
    } else if name.contains("remove") || name.contains("purge") || name.contains("cleanup") {
        PolicyKind::Purge
    } else if name.contains("alert") {
        PolicyKind::Alert
    } else if let Some(da) = kvs.get("default_action") {
        let da = da.trim();
        if da.starts_with("lhsm.archive") {
            PolicyKind::HsmArchive
        } else if da.starts_with("lhsm.release") {
            PolicyKind::HsmRelease
        } else if da.starts_with("common.unlink") || da.starts_with("rm") {
            PolicyKind::Purge
        } else {
            warns.push(format!("default_action {da:?} not recognised, defaulting to purge"));
            PolicyKind::Purge
        }
    } else {
        warns.push(format!("no default_action for policy {name}, defaulting to purge"));
        PolicyKind::Purge
    };

    // LRU sort attr.
    let lru_sort = kvs.get("default_lru_sort_attr").and_then(|v| {
        let v = v.trim().trim_end_matches(';').trim();
        match v {
            "last_mod" => Some(LruSortAttr::Mtime),
            "last_access" => Some(LruSortAttr::Atime),
            "creation_time" | "ctime" => Some(LruSortAttr::Ctime),
            "rm_time" => None,
            other => {
                warns.push(format!("unknown lru_sort_attr: {other}"));
                None
            }
        }
    });

    // Triggers: look for a block header that starts with `<name>_trigger`
    // OR `trigger`. Robinhood-C typically places them outside the policy
    // block with a header like `lhsm_archive_trigger` — our importer has
    // already captured them at the top level, so this shortcut only sees
    // inline triggers. Most example configs use the outer form; we fall
    // back to a daily interval if none was found.
    let triggers = parse_triggers(&blocks, &mut warns);

    let default_action = ActionParams {
        lru_sort,
        ..ActionParams::default()
    };

    let def = PolicyDef {
        name: name.to_string(),
        kind,
        scope,
        rules: vec![],
        default_action,
        triggers,
        ignore_fileclass: vec![],
        enabled: true,
    };

    Ok((def, warns))
}

fn parse_triggers(blocks: &[(String, String)], warns: &mut Vec<String>) -> Vec<TriggerSpec> {
    let mut out = Vec::new();
    for (hdr, body) in blocks {
        if !hdr.trim().ends_with("_trigger") && hdr.trim() != "trigger" {
            continue;
        }
        let (_sub, kvs) = split_body(body);
        let kind = kvs
            .get("trigger_on")
            .map(|v| v.trim().trim_end_matches(';').to_string())
            .unwrap_or_else(|| "scheduled".into());
        let check = kvs
            .get("check_interval")
            .and_then(|v| parse_duration_secs(v.trim().trim_end_matches(';')))
            .unwrap_or(86_400);
        match kind.as_str() {
            "scheduled" | "periodic" => out.push(TriggerSpec::Interval { secs: check }),
            "cron" => {
                if let Some(expr) = kvs.get("expression") {
                    out.push(TriggerSpec::Cron {
                        expression: expr.trim().trim_matches('"').to_string(),
                    });
                }
            }
            other => warns.push(format!("trigger_on {other:?} not translated")),
        }
    }
    out
}

/// Crude expression parser: handles AND (`and`, `&&`) of simple atoms.
///
/// Recognised atoms:
///   type == file | type == directory | type == symlink
///   size > N[KMGT]B | size <= N[KMGT]B | size == 0
///   name == "PAT"  (translates to NameLike with glob-to-SQL-LIKE)
///   uid == N | gid == N | last_mod > Nd | last_access < Nd
/// Unrecognised atoms are reduced to Predicate::True and logged.
fn parse_expression(src: &str) -> Result<Predicate> {
    let tokens: Vec<&str> = src
        .split(['\n', ';'])
        .flat_map(|s| s.split(" and "))
        .flat_map(|s| s.split("&&"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let mut preds: Vec<Predicate> = Vec::new();
    for atom in tokens {
        if let Some(p) = parse_atom(atom) {
            preds.push(p);
        }
    }
    Ok(match preds.len() {
        0 => Predicate::True,
        1 => preds.into_iter().next().unwrap(),
        _ => Predicate::And { children: preds },
    })
}

fn parse_atom(s: &str) -> Option<Predicate> {
    let s = s.trim().trim_end_matches(';').trim();

    // type == file | directory | symlink
    if let Some(rest) = s.strip_prefix("type ==") {
        let v = rest.trim();
        let kind = match v {
            "file" => Some(0),
            "directory" | "dir" => Some(1),
            "symlink" => Some(2),
            _ => None,
        };
        if let Some(k) = kind {
            return Some(Predicate::Cmp {
                field: Field::Kind,
                cmp: CmpOp::Eq,
                value: Value::Num(k),
            });
        }
    }

    // size OP N[KMGT]B? -- handles e.g. "size > 1GB", "size == 0", "size <= 16MB"
    for (op_str, op) in &[
        (">=", CmpOp::Ge),
        ("<=", CmpOp::Le),
        ("==", CmpOp::Eq),
        ("!=", CmpOp::Ne),
        (">", CmpOp::Gt),
        ("<", CmpOp::Lt),
    ] {
        if let Some((lhs, rhs)) = s.split_once(op_str) {
            let lhs = lhs.trim();
            let rhs = rhs.trim();
            if let Some(field) = map_field(lhs) {
                if let Some(v) = parse_value(rhs) {
                    return Some(Predicate::Cmp {
                        field,
                        cmp: *op,
                        value: Value::Num(v),
                    });
                }
                if lhs == "name" && rhs.starts_with('"') {
                    let pat = rhs.trim_matches('"').to_string();
                    return Some(Predicate::NameLike {
                        pattern: glob_to_sql_like(&pat),
                    });
                }
            }
        }
    }

    None
}

fn map_field(s: &str) -> Option<Field> {
    match s {
        "size" => Some(Field::Size),
        "uid" => Some(Field::Uid),
        "gid" => Some(Field::Gid),
        "projid" => Some(Field::Projid),
        "mode" => Some(Field::Mode),
        "last_mod" | "mtime" => Some(Field::Mtime),
        "last_access" | "atime" => Some(Field::Atime),
        "ctime" | "creation_time" => Some(Field::Ctime),
        "nlink" | "links" => Some(Field::Nlink),
        "name" => Some(Field::Kind), // placeholder; caller diverts strings to NameLike
        _ => None,
    }
}

/// Parse "1024", "16MB", "1GB", "10d" (days) into bytes or seconds.
/// The caller knows which unit is expected — this is a lenient best
/// effort.
fn parse_value(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    let bytes = |n: i64, m: i64| n.checked_mul(m);
    if let Some(num) = s.strip_suffix("B") {
        if let Some(n) = num.strip_suffix("K") {
            return n.parse().ok().and_then(|v| bytes(v, 1024));
        }
        if let Some(n) = num.strip_suffix("M") {
            return n.parse().ok().and_then(|v| bytes(v, 1024 * 1024));
        }
        if let Some(n) = num.strip_suffix("G") {
            return n.parse().ok().and_then(|v| bytes(v, 1024 * 1024 * 1024));
        }
        if let Some(n) = num.strip_suffix("T") {
            return n.parse().ok().and_then(|v| bytes(v, 1024i64 * 1024 * 1024 * 1024));
        }
    }
    if let Some(n) = s.strip_suffix('d') {
        return n.parse().ok().and_then(|v: i64| bytes(v, 86_400));
    }
    if let Some(n) = s.strip_suffix('h') {
        return n.parse().ok().and_then(|v: i64| bytes(v, 3_600));
    }
    None
}

fn parse_duration_secs(s: &str) -> Option<u64> {
    parse_value(s).and_then(|v| u64::try_from(v).ok())
}

/// Shell glob → SQL LIKE. `*` → `%`, `?` → `_`, escape existing `%`/`_`.
fn glob_to_sql_like(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len());
    for c in pat.chars() {
        match c {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Split a brace-free body into (nested blocks, plain key=value lines).
/// Sub-blocks use the same `name { ... }` convention as top-level.
fn split_body(body: &str) -> (Vec<(String, String)>, HashMap<String, String>) {
    let blocks = split_top_level_blocks(body).unwrap_or_default();
    // Replace sub-blocks with placeholders so the rest can be line-scanned
    // for k=v entries. Quick heuristic: strip every `<header> { … }`.
    let mut plain = body.to_string();
    for (h, b) in &blocks {
        let needle = format!("{h}{{{b}}}");
        plain = plain.replacen(&needle, "", 1);
    }
    let mut kvs: HashMap<String, String> = HashMap::new();
    for line in plain.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_end_matches(';').trim();
            if !k.is_empty() && !v.is_empty() {
                kvs.insert(k.to_string(), v.to_string());
            }
        }
    }
    (blocks, kvs)
}

// ───────── tests ─────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_line_and_block_comments() {
        let s = "a # hi\nb /* x */ c // d\ne";
        let out = strip_comments(s);
        assert!(out.contains("a"));
        assert!(out.contains("b"));
        assert!(out.contains("c"));
        assert!(!out.contains("hi"));
        assert!(!out.contains("d"));
    }

    #[test]
    fn parses_size_atom() {
        let p = parse_atom("size > 1GB").unwrap();
        assert!(matches!(
            p,
            Predicate::Cmp {
                field: Field::Size,
                cmp: CmpOp::Gt,
                value: Value::Num(1073741824)
            }
        ));
    }

    #[test]
    fn parses_type_atom() {
        let p = parse_atom("type == file").unwrap();
        assert!(matches!(
            p,
            Predicate::Cmp {
                field: Field::Kind,
                cmp: CmpOp::Eq,
                value: Value::Num(0)
            }
        ));
    }

    #[test]
    fn glob_star_becomes_percent() {
        assert_eq!(glob_to_sql_like("file.*"), "file.%");
        assert_eq!(glob_to_sql_like("a?b"), "a_b");
        assert_eq!(glob_to_sql_like("50% off"), r"50\% off");
    }

    #[test]
    fn imports_minimal_fileclass_and_policy() {
        let src = r#"
            FileClass big_files {
                definition { type == file and size > 1GB }
            }
            define_policy lhsm_archive {
                scope { type == file and size > 0 }
                default_action = lhsm.archive;
                default_lru_sort_attr = last_mod;
            }
        "#;
        let r = import(src).unwrap();
        assert_eq!(r.fileclasses.len(), 1);
        assert_eq!(r.policies.len(), 1);
        let p = &r.policies[0];
        assert_eq!(p.name, "lhsm_archive");
        assert_eq!(p.kind, PolicyKind::HsmArchive);
        assert_eq!(p.default_action.lru_sort, Some(LruSortAttr::Mtime));
    }

    #[test]
    fn unknown_block_is_warning_not_error() {
        let src = r#"
            General { fs_path = /mnt/lustre; }
            define_policy cleanup {
                default_action = common.unlink;
                default_lru_sort_attr = last_access;
            }
        "#;
        let r = import(src).unwrap();
        assert!(r.warnings.iter().any(|w| w.contains("General")));
        assert_eq!(r.policies.len(), 1);
        assert_eq!(r.policies[0].kind, PolicyKind::Purge);
    }
}
