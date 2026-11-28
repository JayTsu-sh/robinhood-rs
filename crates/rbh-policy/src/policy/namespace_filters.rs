//! Resolve indexed Lustre link paths for namespace predicates.

use sqlx::{MySql, QueryBuilder};

use super::{Comparison, CompiledPolicy, Condition, invalid};
use crate::PolicyError;
use rbh_predicate::{CmpOp, SqlParam};

pub(super) fn condition(field: &str, op: Comparison, value: &str, location: &str) -> Result<Condition, PolicyError> {
    if value.is_empty()
        || value.len() > 4095
        || value.contains('\0')
        || (field == "object.path" && !value.starts_with('/') && !matches!(op, Comparison::Glob))
    {
        return Err(invalid(format!(
            "{location}.value must be a nonempty path or name pattern"
        )));
    }
    let name = if field == "object.name" {
        "SUBSTRING_INDEX(p.pathname, '/', -1)"
    } else {
        "p.pathname"
    };
    let (predicate, operand) = if matches!(op, Comparison::Glob) {
        let mut pattern = String::new();
        for ch in value.chars() {
            match ch {
                '*' => pattern.push('%'),
                '?' => pattern.push('_'),
                '%' | '_' | '!' => {
                    pattern.push('!');
                    pattern.push(ch);
                }
                ch => pattern.push(ch),
            }
        }
        (format!("BINARY {name} LIKE BINARY ? ESCAPE '!'"), pattern)
    } else {
        (format!("BINARY {name} = BINARY ?"), value.to_owned())
    };
    let negated = if !matches!(op, Comparison::Glob) && matches!(op.equality(location)?, CmpOp::Ne) {
        "NOT "
    } else {
        ""
    };
    Ok(Condition {
        sql: format!("{negated}EXISTS (SELECT 1 FROM policy_paths p WHERE p.fid = entries.object_id AND {predicate})"),
        params: vec![SqlParam::Str(operand)],
        uses_read_heat: false,
        may_be_unknown: false,
        can_match_without_heat: true,
        can_fail_without_heat: true,
    })
}

impl CompiledPolicy {
    pub(crate) fn push_namespace_paths(&self, query: &mut QueryBuilder<MySql>, fid: Option<&[u8]>) {
        if !self.uses_namespace_filters {
            return;
        }
        let migration = self
            .pool_migration()
            .expect("namespace predicates require scoped pool migration");
        let filesystem = self.filesystem.as_str().to_owned();
        // A latin1 container retains each path byte without binary CHAR padding.
        // Filter predicates and live-link verification compare its raw bytes.
        query
            .push(
                "WITH RECURSIVE ancestors (fid, parent_id, pathname, depth) AS (
               SELECT edge.object_id, edge.parent_id,
                   CAST(CONCAT('/', CONVERT(edge.name USING latin1)) AS CHAR(4096) CHARACTER SET latin1), 0
               FROM scoped_namespace_edges edge JOIN scoped_entries f USING (filesystem_id, object_kind, object_id)
               WHERE edge.filesystem_id = ",
            )
            .push_bind(filesystem.clone());
        query.push(" AND edge.object_kind = 0 AND edge.parent_kind = 0 AND f.kind = 0");
        if let Some(fid) = fid {
            query.push(" AND edge.object_id = ").push_bind(fid.to_vec());
        } else {
            query
                .push(" AND BINARY f.pool_name = BINARY ")
                .push_bind(migration.source_pool.clone());
        }
        query
            .push(
                " UNION ALL SELECT ancestors.fid, parent.parent_id,
                 CONCAT('/', CONVERT(parent.name USING latin1), ancestors.pathname), ancestors.depth + 1
               FROM ancestors JOIN scoped_namespace_edges parent ON parent.object_id = ancestors.parent_id
               WHERE parent.filesystem_id = ",
            )
            .push_bind(filesystem.clone());
        query
            .push(
                " AND parent.object_kind = 0 AND parent.parent_kind = 0
              AND ancestors.depth < 998 AND LENGTH(parent.name) + LENGTH(ancestors.pathname) < 4095
             ), policy_paths AS (
               SELECT ancestors.fid, ancestors.pathname FROM ancestors
               JOIN scoped_entries root ON root.object_id = ancestors.parent_id
               WHERE root.filesystem_id = ",
            )
            .push_bind(filesystem);
        query.push(" AND root.object_kind = 0 AND root.kind = 1 AND root.parent_id IS NULL) ");
    }
}
