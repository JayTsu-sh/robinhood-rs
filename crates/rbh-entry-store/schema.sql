-- Initialize an empty Robinhood database on MariaDB 11.
-- Import this file before starting the daemon. The completion marker is last.

-- Compatibility catalog for /api/compat/lustre.

CREATE TABLE `entries` (
  `fid` binary(16) NOT NULL,
  `parent_fid` binary(16) DEFAULT NULL,
  `name` varbinary(255) DEFAULT NULL,
  `kind` tinyint unsigned NOT NULL COMMENT '0=file,1=dir,2=symlink,3=chardev,4=blockdev,5=fifo,6=socket',
  `size` bigint unsigned NOT NULL DEFAULT 0,
  `blocks` bigint unsigned NOT NULL DEFAULT 0,
  `uid` int unsigned NOT NULL DEFAULT 0,
  `gid` int unsigned NOT NULL DEFAULT 0,
  `projid` int unsigned NOT NULL DEFAULT 0,
  `mode` int unsigned NOT NULL DEFAULT 0,
  `nlink` int unsigned NOT NULL DEFAULT 1,
  `atime` bigint NOT NULL DEFAULT 0 COMMENT 'UNIX seconds',
  `mtime` bigint NOT NULL DEFAULT 0 COMMENT 'UNIX seconds',
  `ctime` bigint NOT NULL DEFAULT 0 COMMENT 'UNIX seconds',
  `stripe_count` smallint unsigned DEFAULT NULL,
  `stripe_size` int unsigned DEFAULT NULL,
  `pool_name` varchar(64) DEFAULT NULL,
  `sm_status` JSON DEFAULT NULL COMMENT 'Per-status-manager state keyed by SM name',
  `last_seen` bigint NOT NULL DEFAULT 0 COMMENT 'UNIX seconds of last observation',
  `depth` int unsigned NOT NULL DEFAULT 0,
  `hsm_state` varchar(16) GENERATED ALWAYS AS (json_unquote(json_extract(`sm_status`,'$.hsm_state'))) VIRTUAL,
  PRIMARY KEY (`fid`),
  KEY `idx_parent` (`parent_fid`),
  KEY `idx_uid` (`uid`),
  KEY `idx_gid` (`gid`),
  KEY `idx_projid` (`projid`),
  KEY `idx_type` (`kind`),
  KEY `idx_mtime` (`mtime`),
  KEY `idx_size` (`size`),
  KEY `idx_last_seen` (`last_seen`),
  KEY `idx_entries_hsm_state` (`hsm_state`),
  KEY `idx_entries_atime` (`atime`),
  KEY `idx_depth` (`depth`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `names` (
  `fid` binary(16) NOT NULL,
  `parent_fid` binary(16) NOT NULL,
  `name` varbinary(255) NOT NULL,
  PRIMARY KEY (`fid`,`parent_fid`,`name`),
  KEY `idx_names_parent` (`parent_fid`),
  KEY `idx_names_parent_name` (`parent_fid`,`name`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `stripe_items` (
  `fid` binary(16) NOT NULL,
  `stripe_index` smallint unsigned NOT NULL COMMENT 'Stripe number within the file',
  `ost_index` int unsigned NOT NULL COMMENT 'OST index holding this stripe',
  PRIMARY KEY (`fid`,`stripe_index`),
  KEY `idx_stripe_ost` (`ost_index`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `removed_entries` (
  `fid` binary(16) NOT NULL,
  `parent_fid` binary(16) DEFAULT NULL,
  `name` varbinary(255) DEFAULT NULL,
  `kind` tinyint unsigned NOT NULL,
  `size` bigint unsigned NOT NULL DEFAULT 0,
  `uid` int unsigned NOT NULL DEFAULT 0,
  `gid` int unsigned NOT NULL DEFAULT 0,
  `sm_status` JSON DEFAULT NULL,
  `rm_time` bigint NOT NULL COMMENT 'UNIX seconds when UNLNK was observed',
  PRIMARY KEY (`fid`),
  KEY `idx_rm_time` (`rm_time`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

-- Filesystem catalog and resource observations.

CREATE TABLE `filesystems` (
  `id` varchar(64) NOT NULL,
  `backend_kind` varchar(16) NOT NULL,
  `mount_path` varbinary(4096) NOT NULL,
  `capabilities` JSON NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `scoped_entries` (
  `filesystem_id` varchar(64) NOT NULL,
  `object_kind` tinyint unsigned NOT NULL COMMENT '0=Lustre FID, 1=JuiceFS inode',
  `object_id` binary(16) NOT NULL,
  `entry_data` JSON NOT NULL COMMENT 'Filesystem-scoped entry payload',
  `parent_kind` tinyint unsigned DEFAULT NULL,
  `parent_id` binary(16) DEFAULT NULL,
  `fid` binary(16) DEFAULT NULL COMMENT 'Lustre compatibility projection; NULL for JuiceFS',
  `name` varbinary(255) NOT NULL DEFAULT '',
  `kind` tinyint unsigned NOT NULL DEFAULT 0,
  `size` bigint unsigned NOT NULL DEFAULT 0,
  `blocks` bigint unsigned NOT NULL DEFAULT 0,
  `uid` int unsigned NOT NULL DEFAULT 0,
  `gid` int unsigned NOT NULL DEFAULT 0,
  `projid` int unsigned NOT NULL DEFAULT 0,
  `mode` int unsigned NOT NULL DEFAULT 0,
  `nlink` int unsigned NOT NULL DEFAULT 1,
  `atime` bigint NOT NULL DEFAULT 0,
  `mtime` bigint NOT NULL DEFAULT 0,
  `ctime` bigint NOT NULL DEFAULT 0,
  `stripe_count` smallint unsigned DEFAULT NULL,
  `stripe_size` int unsigned DEFAULT NULL,
  `pool_name` varchar(64) DEFAULT NULL,
  `sm_status` JSON DEFAULT NULL,
  `last_seen` bigint NOT NULL DEFAULT 0,
  `depth` int unsigned NOT NULL DEFAULT 0,
  `layout_observed_at` bigint DEFAULT NULL,
  `pool_entered_at` bigint DEFAULT NULL,
  `lustre_link_count_known` tinyint(1) NOT NULL DEFAULT 1,
  PRIMARY KEY (`filesystem_id`,`object_kind`,`object_id`),
  KEY `idx_scoped_parent` (`filesystem_id`,`parent_kind`,`parent_id`),
  KEY `idx_scoped_uid` (`filesystem_id`,`uid`),
  KEY `idx_scoped_gid` (`filesystem_id`,`gid`),
  KEY `idx_scoped_projid` (`filesystem_id`,`projid`),
  KEY `idx_scoped_type` (`filesystem_id`,`kind`),
  KEY `idx_scoped_mtime` (`filesystem_id`,`mtime`),
  KEY `idx_scoped_size` (`filesystem_id`,`size`),
  KEY `idx_scoped_last_seen` (`filesystem_id`,`last_seen`),
  CONSTRAINT `fk_scoped_entry_filesystem` FOREIGN KEY (`filesystem_id`) REFERENCES `filesystems` (`id`) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `scoped_namespace_edges` (
  `filesystem_id` varchar(64) NOT NULL,
  `parent_kind` tinyint unsigned NOT NULL,
  `parent_id` binary(16) NOT NULL,
  `name` varbinary(255) NOT NULL,
  `object_kind` tinyint unsigned NOT NULL,
  `object_id` binary(16) NOT NULL,
  PRIMARY KEY (`filesystem_id`,`parent_kind`,`parent_id`,`name`),
  KEY `idx_scoped_edge_object` (`filesystem_id`,`object_kind`,`object_id`),
  CONSTRAINT `fk_scoped_edge_filesystem` FOREIGN KEY (`filesystem_id`) REFERENCES `filesystems` (`id`) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `scoped_removed_entries` (
  `filesystem_id` varchar(64) NOT NULL,
  `object_kind` tinyint unsigned NOT NULL,
  `object_id` binary(16) NOT NULL,
  `entry_data` JSON NOT NULL,
  `rm_time` bigint NOT NULL,
  PRIMARY KEY (`filesystem_id`,`object_kind`,`object_id`),
  KEY `idx_scoped_removed_time` (`filesystem_id`,`rm_time`),
  CONSTRAINT `fk_scoped_removed_filesystem` FOREIGN KEY (`filesystem_id`) REFERENCES `filesystems` (`id`) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `scoped_stripe_items` (
  `filesystem_id` varchar(64) NOT NULL,
  `object_kind` tinyint unsigned NOT NULL,
  `object_id` binary(16) NOT NULL,
  `stripe_index` smallint unsigned NOT NULL,
  `ost_index` int unsigned NOT NULL,
  PRIMARY KEY (`filesystem_id`,`object_kind`,`object_id`,`stripe_index`),
  KEY `idx_scoped_stripe_ost` (`filesystem_id`,`ost_index`),
  CONSTRAINT `fk_scoped_stripe_filesystem` FOREIGN KEY (`filesystem_id`) REFERENCES `filesystems` (`id`) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `filesystem_baselines` (
  `filesystem_id` varchar(64) NOT NULL,
  `state` varchar(16) NOT NULL,
  `scan_started_at` bigint DEFAULT NULL,
  `completed_at` bigint DEFAULT NULL,
  `last_version` bigint unsigned DEFAULT NULL,
  `invalid_reason` varchar(255) DEFAULT NULL,
  PRIMARY KEY (`filesystem_id`),
  CONSTRAINT `fk_baseline_filesystem` FOREIGN KEY (`filesystem_id`) REFERENCES `filesystems` (`id`) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `changelog_cursor` (
  `filesystem_id` varchar(64) NOT NULL,
  `mdt_name` varchar(64) NOT NULL,
  `reader_id` varchar(16) DEFAULT NULL COMMENT 'e.g. cl3 — the pre-registered changelog user',
  `last_rec` bigint unsigned NOT NULL DEFAULT 0 COMMENT 'Last durably committed record index',
  `updated_at` bigint NOT NULL DEFAULT 0 COMMENT 'UNIX seconds',
  UNIQUE KEY `uq_changelog_cursor_filesystem_mdt` (`filesystem_id`,`mdt_name`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `filesystem_resource_snapshots` (
  `observer_id` varbinary(255) NOT NULL,
  `filesystem_id` varchar(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  `revision` bigint unsigned NOT NULL DEFAULT 1,
  `observed_at` bigint unsigned NOT NULL,
  `observation` JSON NOT NULL,
  PRIMARY KEY (`observer_id`,`filesystem_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `lustre_read_heat` (
  `filesystem_id` varchar(64) NOT NULL,
  `object_kind` tinyint unsigned NOT NULL DEFAULT 0,
  `object_id` binary(16) NOT NULL,
  `client_id` varchar(63) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  `profile_hash` binary(32) NOT NULL,
  `observed_at` datetime(6) NOT NULL,
  `read_samples` bigint unsigned DEFAULT NULL,
  `read_bytes` bigint unsigned DEFAULT NULL,
  `period_seconds` int unsigned DEFAULT NULL,
  `decay_percentage` tinyint unsigned DEFAULT NULL,
  `failure` varchar(48) DEFAULT NULL,
  PRIMARY KEY (`filesystem_id`,`object_kind`,`object_id`,`client_id`),
  CONSTRAINT `fk_lustre_read_heat_entry` FOREIGN KEY (`filesystem_id`, `object_kind`, `object_id`) REFERENCES `scoped_entries` (`filesystem_id`, `object_kind`, `object_id`) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

-- Configuration and policy execution.

CREATE TABLE `classifiers` (
  `id` bigint unsigned NOT NULL AUTO_INCREMENT,
  `name` varchar(255) NOT NULL,
  `definition` JSON NOT NULL,
  `enabled` tinyint(1) NOT NULL DEFAULT 1,
  `created_at` datetime(6) NOT NULL DEFAULT current_timestamp(6),
  `updated_at` datetime(6) NOT NULL DEFAULT current_timestamp(6) ON UPDATE current_timestamp(6),
  PRIMARY KEY (`id`),
  UNIQUE KEY `name` (`name`),
  KEY `idx_classifiers_enabled` (`enabled`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policies` (
  `id` bigint unsigned NOT NULL AUTO_INCREMENT,
  `name` varchar(255) DEFAULT NULL,
  `definition` JSON NOT NULL,
  `enabled` tinyint(1) NOT NULL DEFAULT 1,
  `created_at` datetime(6) NOT NULL DEFAULT current_timestamp(6),
  `updated_at` datetime(6) NOT NULL DEFAULT current_timestamp(6) ON UPDATE current_timestamp(6),
  `revision` bigint unsigned NOT NULL,
  `next_run` datetime(6) DEFAULT NULL,
  `heat_filesystem` varchar(64) DEFAULT NULL,
  PRIMARY KEY (`id`),
  UNIQUE KEY `name` (`name`),
  KEY `idx_policies_enabled` (`enabled`),
  KEY `idx_policy_due` (`next_run`,`id`),
  KEY `idx_policy_heat` (`heat_filesystem`,`enabled`,`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_revisions` (
  `policy_id` bigint unsigned NOT NULL,
  `revision` bigint unsigned NOT NULL,
  `definition` JSON NOT NULL,
  `resource_bindings` JSON DEFAULT NULL,
  `modified_by` varchar(255) DEFAULT NULL,
  PRIMARY KEY (`policy_id`,`revision`),
  CONSTRAINT `fk_policy_revisions_policy` FOREIGN KEY (`policy_id`) REFERENCES `policies` (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_configuration_lock` (
  `id` tinyint unsigned NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_resource_heads` (
  `kind` varchar(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  `name` varchar(253) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  `revision` bigint unsigned NOT NULL,
  `deleted` tinyint(1) NOT NULL DEFAULT 0,
  PRIMARY KEY (`kind`,`name`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_resource_revisions` (
  `kind` varchar(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  `name` varchar(253) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  `revision` bigint unsigned NOT NULL,
  `definition` JSON DEFAULT NULL,
  `modified_by` varchar(255) NOT NULL,
  PRIMARY KEY (`kind`,`name`,`revision`),
  CONSTRAINT `policy_resource_revisions_ibfk_1` FOREIGN KEY (`kind`, `name`) REFERENCES `policy_resource_heads` (`kind`, `name`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_worker_lock` (
  `id` tinyint unsigned NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `console_lustre_resources` (
  `observer_id` varbinary(255) NOT NULL,
  `filesystem_id` varchar(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  `metadata` JSON NOT NULL,
  PRIMARY KEY (`observer_id`, `filesystem_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_runs` (
  `id` bigint unsigned NOT NULL AUTO_INCREMENT,
  `policy_id` bigint unsigned NOT NULL,
  `revision` bigint unsigned NOT NULL,
  `state` enum('queued','running','previewed','finished','deferred','cancelled','paused','awaiting_approval') NOT NULL DEFAULT 'queued',
  `reason` varchar(255) DEFAULT NULL,
  `candidate_count` bigint unsigned DEFAULT NULL,
  `estimated_bytes` bigint unsigned DEFAULT NULL,
  `cursor_id` binary(16) DEFAULT NULL,
  `cursor_value` bigint DEFAULT NULL,
  `paused_millis` bigint unsigned NOT NULL DEFAULT 0,
  `requested_by` varchar(255) DEFAULT NULL,
  `approved_by` varchar(255) DEFAULT NULL,
  `created_at` datetime(6) NOT NULL DEFAULT current_timestamp(6),
  `started_at` datetime(6) DEFAULT NULL,
  `updated_at` datetime(6) NOT NULL DEFAULT current_timestamp(6) ON UPDATE current_timestamp(6),
  `heat_deferred_by_reason` JSON DEFAULT NULL,
  `directory_scope` JSON DEFAULT NULL,
  PRIMARY KEY (`id`),
  KEY `policy_id` (`policy_id`,`revision`),
  KEY `idx_policy_runs_policy` (`policy_id`,`id`),
  KEY `idx_policy_runs_queue` (`state`,`updated_at`,`id`),
  KEY `idx_policy_runs_state_id` (`state`,`id`),
  KEY `idx_policy_runs_policy_state_id` (`policy_id`,`state`,`id`),
  CONSTRAINT `policy_runs_ibfk_1` FOREIGN KEY (`policy_id`, `revision`) REFERENCES `policy_revisions` (`policy_id`, `revision`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_operations` (
  `id` bigint unsigned NOT NULL AUTO_INCREMENT,
  `run_id` bigint unsigned NOT NULL,
  `filesystem` varchar(64) NOT NULL,
  `fid` binary(16) NOT NULL,
  `action` varchar(64) NOT NULL,
  `archive_id` int unsigned NOT NULL DEFAULT 0,
  `state` enum('submitting','submitted','unknown','completed','failed','deferred') NOT NULL,
  `reason` varchar(255) DEFAULT NULL,
  `execution_profile` varchar(253) DEFAULT NULL,
  `terminal_error_class` varchar(32) DEFAULT NULL,
  `finished_at` datetime(6) DEFAULT NULL,
  `completion_evidence` JSON DEFAULT NULL,
  `attempted` tinyint(1) NOT NULL DEFAULT 0,
  `attempt_count` int unsigned DEFAULT NULL,
  `attempts_reserved` int unsigned NOT NULL DEFAULT 1,
  `migration_lease_until` datetime(6) DEFAULT NULL,
  `generation` JSON DEFAULT NULL,
  `principal` JSON DEFAULT NULL,
  `size` bigint unsigned NOT NULL,
  `accepted_at` datetime(6) DEFAULT NULL,
  `checked_at` datetime(6) DEFAULT NULL,
  `pending_fid` binary(16) GENERATED ALWAYS AS (case when `state` in ('submitting','submitted','unknown') then `fid` else NULL end) STORED,
  `migration_plan` JSON DEFAULT NULL,
  `paths` JSON DEFAULT NULL,
  `pending_migration_fs` varchar(64) GENERATED ALWAYS AS (case when `migration_plan` is not null and `state` in ('submitting','submitted','unknown') then `filesystem` else NULL end) STORED,
  PRIMARY KEY (`id`),
  UNIQUE KEY `run_id` (`run_id`,`fid`),
  UNIQUE KEY `filesystem` (`filesystem`,`pending_fid`),
  KEY `idx_policy_migration_fs` (`pending_migration_fs`),
  KEY `idx_policy_operations_pending` (`pending_fid`,`checked_at`,`id`),
  KEY `idx_policy_profile_pending` (`execution_profile`,`pending_fid`),
  KEY `idx_policy_operations_run_id` (`run_id`,`id`),
  KEY `idx_policy_operations_run_state_id` (`run_id`,`state`,`id`),
  CONSTRAINT `policy_operations_ibfk_1` FOREIGN KEY (`run_id`) REFERENCES `policy_runs` (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_run_controls` (
  `id` bigint unsigned NOT NULL AUTO_INCREMENT,
  `run_id` bigint unsigned NOT NULL,
  `action` enum('pause','resume','cancel','approve') NOT NULL,
  `actor` varchar(255) NOT NULL,
  `occurred_at` datetime(6) NOT NULL DEFAULT current_timestamp(6),
  PRIMARY KEY (`id`),
  KEY `idx_run_controls_run` (`run_id`,`id`),
  CONSTRAINT `fk_run_controls_run` FOREIGN KEY (`run_id`) REFERENCES `policy_runs` (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

CREATE TABLE `policy_breakers` (
  `policy_id` bigint unsigned NOT NULL,
  `opened` tinyint(1) NOT NULL DEFAULT 0,
  `reset_by` varchar(255) DEFAULT NULL,
  `reset_at` datetime(6) DEFAULT NULL,
  PRIMARY KEY (`policy_id`),
  CONSTRAINT `policy_breakers_ibfk_1` FOREIGN KEY (`policy_id`) REFERENCES `policies` (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_uca1400_ai_ci;

INSERT INTO policy_configuration_lock (id) VALUES (1);
INSERT INTO policy_worker_lock (id) VALUES (1);

-- Record completion only after the tables and required rows exist.
-- Keep the version and required table checks in src/schema.rs in sync with this file.
CREATE TABLE rbh_schema (
  id tinyint unsigned NOT NULL PRIMARY KEY CHECK (id = 1),
  version int unsigned NOT NULL
) ENGINE=InnoDB;
INSERT INTO rbh_schema (id, version) VALUES (1, 2);
