//! Semver-style version constraints for module bootstrap (e.g. `0.1.*`, `0.3.2`).

use crate::module::traits::ModuleError;

/// Parse `major.minor.patch`, ignoring a leading `v` and any pre-release suffix.
pub fn parse_semver_triple(version: &str) -> Option<(u64, u64, u64)> {
    let version = version.trim().trim_start_matches('v');
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Compare two semver triples for ordering (higher = newer).
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let va = parse_semver_triple(a).unwrap_or((0, 0, 0));
    let vb = parse_semver_triple(b).unwrap_or((0, 0, 0));
    va.cmp(&vb)
}

/// Return true when `version` satisfies `constraint`.
///
/// Supported constraints:
/// - `*` or empty — any version
/// - `0.1.2` — exact match
/// - `0.1.*` — same major/minor, any patch
/// - `0.*` — same major, any minor/patch
pub fn matches_version_constraint(version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();
    if constraint.is_empty() || constraint == "*" {
        return true;
    }
    let Some(v) = parse_semver_triple(version) else {
        return false;
    };

    let constraint = constraint.trim_start_matches('v');
    let segments: Vec<&str> = constraint.split('.').collect();
    if segments.is_empty() {
        return false;
    }

    for (i, seg) in segments.iter().enumerate() {
        if *seg == "*" {
            return true;
        }
        let v_part = match i {
            0 => v.0,
            1 => v.1,
            2 => v.2,
            _ => return false,
        };
        let Ok(c_part) = seg.parse::<u64>() else {
            return false;
        };
        if v_part != c_part {
            return false;
        }
    }

    // Trailing segments in the version beyond the constraint are allowed when
    // the constraint ends on a fixed component (e.g. constraint `0.1` matches `0.1.5`).
    true
}

/// Pick the highest release version that satisfies `constraint`.
pub fn select_highest_matching_version(
    versions: &[String],
    constraint: &str,
) -> Result<String, ModuleError> {
    let constraint = constraint.trim();
    if constraint.is_empty() || constraint == "*" {
        return Err(ModuleError::OperationError(
            "select_highest_matching_version requires a non-wildcard constraint".to_string(),
        ));
    }

    let mut matches: Vec<&String> = versions
        .iter()
        .filter(|v| matches_version_constraint(v, constraint))
        .collect();

    if matches.is_empty() {
        return Err(ModuleError::OperationError(format!(
            "No release version matches constraint '{constraint}' (available: {versions:?})"
        )));
    }

    matches.sort_by(|a, b| compare_versions(a, b));
    Ok(matches.pop().unwrap().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_version_match() {
        assert!(matches_version_constraint("0.1.2", "0.1.2"));
        assert!(!matches_version_constraint("0.1.3", "0.1.2"));
    }

    #[test]
    fn patch_wildcard() {
        assert!(matches_version_constraint("0.1.0", "0.1.*"));
        assert!(matches_version_constraint("0.1.99", "0.1.*"));
        assert!(!matches_version_constraint("0.2.0", "0.1.*"));
    }

    #[test]
    fn minor_wildcard() {
        assert!(matches_version_constraint("0.3.1", "0.*"));
        assert!(matches_version_constraint("0.0.9", "0.*"));
        assert!(!matches_version_constraint("1.0.0", "0.*"));
    }

    #[test]
    fn star_matches_any() {
        assert!(matches_version_constraint("9.9.9", "*"));
        assert!(matches_version_constraint("0.0.1", ""));
    }

    #[test]
    fn leading_v_stripped() {
        assert!(matches_version_constraint("v0.1.2", "0.1.*"));
    }

    #[test]
    fn select_highest_matching() {
        let versions = vec![
            "0.1.0".to_string(),
            "0.1.5".to_string(),
            "0.2.0".to_string(),
        ];
        assert_eq!(
            select_highest_matching_version(&versions, "0.1.*").unwrap(),
            "0.1.5"
        );
    }

    #[test]
    fn parse_semver_triple_rejects_extra_segments() {
        assert!(parse_semver_triple("1.2.3.4").is_none());
    }

    #[test]
    fn parse_semver_triple_strips_prerelease() {
        assert_eq!(parse_semver_triple("1.2.3-rc1"), Some((1, 2, 3)));
    }

    #[test]
    fn compare_versions_orders_semver() {
        assert_eq!(
            compare_versions("0.1.10", "0.1.2"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn select_highest_matching_no_match_errors() {
        let versions = vec!["0.2.0".to_string()];
        assert!(select_highest_matching_version(&versions, "0.1.*").is_err());
    }
}
