//! Parse human-readable trigger strings into [`TriggerSpec`].
//!
//! Supported formats:
//!
//! | Input            | TriggerSpec                        |
//! |------------------|------------------------------------|
//! | `"1h"`           | `Interval { secs: 3600 }`          |
//! | `"30m"`          | `Interval { secs: 1800 }`          |
//! | `"10s"`          | `Interval { secs: 10 }`            |
//! | `"cron:0 2 * * *"` | `Cron { expression }`            |
//! | `"fs > 85%"`     | `ThresholdFsPct { high_pct: 85 }` |
//! | `"count > 10000"` | `ThresholdCount { high_count }`  |
//! | `"volume > 100GB"` | `ThresholdVolume { high_bytes }` |
//! | `"ost > 80%"`    | `ThresholdOstPct { high_pct: 80 }` |

use crate::model::TriggerSpec;

#[derive(Debug, thiserror::Error)]
pub enum TriggerParseError {
    #[error("empty trigger string")]
    Empty,
    #[error("unknown trigger format: {0:?}")]
    Unknown(String),
    #[error("invalid number in trigger: {0}")]
    BadNumber(String),
    #[error("invalid cron expression: {0}")]
    BadCron(String),
}

/// Parse a human-readable trigger string into a [`TriggerSpec`].
pub fn parse_trigger(s: &str) -> Result<TriggerSpec, TriggerParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(TriggerParseError::Empty);
    }

    // Interval: "1h", "30m", "10s", "3600s"
    if let Some(spec) = try_parse_interval(s)? {
        return Ok(spec);
    }

    // Cron: "cron:0 2 * * *" or "cron: 0 2 * * *"
    if let Some(expr) = s.strip_prefix("cron:") {
        let expr = expr.trim().to_string();
        // 5-field or 6-field — accept both, scheduler-rs handles validation
        if expr.is_empty() {
            return Err(TriggerParseError::BadCron(s.to_string()));
        }
        // Expand 5-field to 6-field (prepend seconds=0)
        let parts: Vec<&str> = expr.split_whitespace().collect();
        let expression = if parts.len() == 5 { format!("0 {expr}") } else { expr };
        return Ok(TriggerSpec::Cron { expression });
    }

    // Threshold: "fs > 85%", "count > 10000", "volume > 100GB", "ost > 80%"
    if let Some(spec) = try_parse_threshold(s)? {
        return Ok(spec);
    }

    Err(TriggerParseError::Unknown(s.to_string()))
}

fn try_parse_interval(s: &str) -> Result<Option<TriggerSpec>, TriggerParseError> {
    // Strip whitespace and try suffixes: h, m, s
    let lower = s.to_lowercase();
    let (num_str, multiplier) = if let Some(n) = lower.strip_suffix('h') {
        (n, 3600u64)
    } else if let Some(n) = lower.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = lower.strip_suffix('s') {
        (n, 1u64)
    } else {
        return Ok(None);
    };
    let n: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| TriggerParseError::BadNumber(s.to_string()))?;
    Ok(Some(TriggerSpec::Interval { secs: n * multiplier }))
}

fn try_parse_threshold(s: &str) -> Result<Option<TriggerSpec>, TriggerParseError> {
    // Normalize: remove spaces around '>' and '%'
    let parts: Vec<&str> = s.splitn(2, '>').collect();
    if parts.len() != 2 {
        return Ok(None);
    }
    let lhs = parts[0].trim().to_lowercase();
    let rhs = parts[1].trim();

    // Default check_interval_secs for all threshold triggers
    const DEFAULT_CHECK_SECS: u64 = 300;

    match lhs.as_str() {
        "fs" => {
            let pct = parse_pct(rhs, s)?;
            Ok(Some(TriggerSpec::ThresholdFsPct {
                check_interval_secs: DEFAULT_CHECK_SECS,
                high_pct: pct,
                low_pct: 0,
                post_trigger_wait_secs: 0,
            }))
        }
        "ost" => {
            let pct = parse_pct(rhs, s)?;
            Ok(Some(TriggerSpec::ThresholdOstPct {
                check_interval_secs: DEFAULT_CHECK_SECS,
                high_pct: pct,
                low_pct: 0,
                post_trigger_wait_secs: 0,
                target: crate::model::ThresholdTarget::Fs,
            }))
        }
        "count" => {
            let n: u64 = rhs
                .trim()
                .parse()
                .map_err(|_| TriggerParseError::BadNumber(s.to_string()))?;
            Ok(Some(TriggerSpec::ThresholdCount {
                check_interval_secs: DEFAULT_CHECK_SECS,
                high_count: n,
                low_count: 0,
                post_trigger_wait_secs: 0,
                target: crate::model::ThresholdTarget::Fs,
            }))
        }
        "volume" => {
            let bytes = parse_size(rhs, s)?;
            Ok(Some(TriggerSpec::ThresholdVolume {
                check_interval_secs: DEFAULT_CHECK_SECS,
                high_bytes: bytes,
                low_bytes: 0,
                post_trigger_wait_secs: 0,
                target: crate::model::ThresholdTarget::Fs,
            }))
        }
        _ => Ok(None),
    }
}

fn parse_pct(s: &str, orig: &str) -> Result<u32, TriggerParseError> {
    let s = s.trim().trim_end_matches('%').trim();
    s.parse::<u32>()
        .map_err(|_| TriggerParseError::BadNumber(orig.to_string()))
}

fn parse_size(s: &str, orig: &str) -> Result<u64, TriggerParseError> {
    let s = s.trim().to_uppercase();
    let (num_str, mult) = if let Some(n) = s.strip_suffix("TB") {
        (n, 1u64 << 40)
    } else if let Some(n) = s.strip_suffix("GB") {
        (n, 1u64 << 30)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n, 1u64 << 20)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n, 1u64 << 10)
    } else {
        (s.as_str(), 1u64)
    };
    let n: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| TriggerParseError::BadNumber(orig.to_string()))?;
    Ok(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_hours() {
        assert_eq!(parse_trigger("1h").unwrap(), TriggerSpec::Interval { secs: 3600 });
        assert_eq!(parse_trigger("2h").unwrap(), TriggerSpec::Interval { secs: 7200 });
    }

    #[test]
    fn interval_minutes() {
        assert_eq!(parse_trigger("30m").unwrap(), TriggerSpec::Interval { secs: 1800 });
    }

    #[test]
    fn interval_seconds() {
        assert_eq!(parse_trigger("10s").unwrap(), TriggerSpec::Interval { secs: 10 });
    }

    #[test]
    fn cron_five_field_expanded() {
        match parse_trigger("cron:0 2 * * *").unwrap() {
            TriggerSpec::Cron { expression } => assert_eq!(expression, "0 0 2 * * *"),
            _ => panic!(),
        }
    }

    #[test]
    fn cron_six_field_kept() {
        match parse_trigger("cron:0 0 2 * * *").unwrap() {
            TriggerSpec::Cron { expression } => assert_eq!(expression, "0 0 2 * * *"),
            _ => panic!(),
        }
    }

    #[test]
    fn threshold_fs_pct() {
        match parse_trigger("fs > 85%").unwrap() {
            TriggerSpec::ThresholdFsPct { high_pct: 85, .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn threshold_ost_pct() {
        match parse_trigger("ost > 80%").unwrap() {
            TriggerSpec::ThresholdOstPct { high_pct: 80, .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn threshold_count() {
        match parse_trigger("count > 10000").unwrap() {
            TriggerSpec::ThresholdCount { high_count: 10000, .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn threshold_volume_gb() {
        match parse_trigger("volume > 100GB").unwrap() {
            TriggerSpec::ThresholdVolume { high_bytes, .. } => {
                assert_eq!(high_bytes, 100u64 << 30)
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn empty_is_error() {
        assert!(matches!(parse_trigger(""), Err(TriggerParseError::Empty)));
    }

    #[test]
    fn unknown_is_error() {
        assert!(matches!(parse_trigger("weekly"), Err(TriggerParseError::Unknown(_))));
    }
}
