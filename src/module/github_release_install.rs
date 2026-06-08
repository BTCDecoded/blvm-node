//! Resolve official module binaries from GitHub Releases.
//!
//! Bootstrap uses `registry/modules.json` **`repo`** (`owner/name`), the semantic
//! **`version`** from `module.toml`, and a **`sha256sums.txt`** asset attached to
//! the matching release tag (`v` + version). Download URLs are derived; hashes
//! come only from that checksum file (same as release CI publishes).

use crate::module::traits::ModuleError;

/// Tag name on GitHub (e.g. `v0.1.2`).
pub fn release_tag(version: &str) -> String {
    format!("v{}", version.trim())
}

/// Release artifact base name for the node platform key (e.g. `x86_64-linux`).
pub fn artifact_name(module_name: &str, platform: &str) -> Result<String, ModuleError> {
    let suffix = match platform {
        "x86_64-linux" => "-x86_64-linux",
        "aarch64-linux" => "-aarch64-linux",
        "x86_64-windows" => "-x86_64-windows.exe",
        "x86_64-apple" => "-x86_64-apple",
        "aarch64-apple" => "-aarch64-apple",
        _ => {
            return Err(ModuleError::OperationError(format!(
                "Unsupported platform for GitHub release bootstrap: {platform}"
            )));
        }
    };
    Ok(format!("{module_name}{suffix}"))
}

/// `https://github.com/{owner}/{repo}/releases/download/{tag}/{filename}`
pub fn release_download_url(github_repo: &str, tag: &str, filename: &str) -> String {
    format!("https://github.com/{github_repo}/releases/download/{tag}/{filename}")
}

/// Checksums file names to try (release workflows publish `sha256sums.txt`).
pub const CHECKSUM_FILENAMES: &[&str] = &["sha256sums.txt", "SHA256SUMS"];

/// Default Git ref for `module.toml` when the registry omits `module_toml_url` (repo-root convention).
pub const DEFAULT_MODULE_MANIFEST_REF: &str = "main";

/// Raw GitHub URL for `module.toml` at the repository root (`owner/name` + ref), like a `Cargo.toml` layout.
pub fn default_module_toml_raw_url(github_repo: &str, git_ref: &str) -> String {
    let github_repo = github_repo.trim();
    let git_ref = git_ref.trim();
    format!("https://raw.githubusercontent.com/{github_repo}/{git_ref}/module.toml")
}

/// Parse GNU `sha256sum` output for a single file name.
pub fn sha256_from_checksums(content: &str, filename: &str) -> Result<String, ModuleError> {
    let wanted = filename.trim();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next().ok_or_else(|| {
            ModuleError::OperationError("Invalid sha256sums line (no hash)".to_string())
        })?;
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let path_field = parts.collect::<Vec<_>>().join(" ");
        if path_field.is_empty() {
            continue;
        }
        let path_field = path_field.strip_prefix('*').unwrap_or(&path_field);
        let base = std::path::Path::new(path_field)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path_field);
        if base == wanted || path_field == wanted || path_field.ends_with(wanted) {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    Err(ModuleError::OperationError(format!(
        "No SHA256 line for '{wanted}' in checksums file"
    )))
}

/// `owner/repo` from registry index.
pub fn validate_github_repo(repo: &str) -> Result<(), ModuleError> {
    let repo = repo.trim();
    if repo.is_empty() {
        return Err(ModuleError::OperationError(
            "Registry entry missing non-empty `repo` (expected owner/repo)".to_string(),
        ));
    }
    let parts: Vec<&str> = repo.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() != 2 {
        return Err(ModuleError::OperationError(format!(
            "Registry `repo` must be `owner/name`, got: {repo}"
        )));
    }
    Ok(())
}

/// Host platform key for GitHub release artifacts (e.g. `x86_64-linux`).
#[cfg(feature = "governance")]
pub fn host_platform_key() -> Result<&'static str, ModuleError> {
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        Ok("x86_64-linux")
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        Ok("aarch64-linux")
    }
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        Ok("x86_64-windows")
    }
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    {
        Ok("x86_64-apple")
    }
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        Ok("aarch64-apple")
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", target_os = "linux"),
        all(target_arch = "x86_64", target_os = "windows"),
        all(target_arch = "x86_64", target_os = "macos"),
        all(target_arch = "aarch64", target_os = "macos"),
    )))]
    {
        Err(ModuleError::OperationError(
            "GitHub release layout is not supported on this host triple".to_string(),
        ))
    }
}

/// Look up `owner/name` for a module in the registry index (`modules.json`).
#[cfg(feature = "governance")]
pub async fn fetch_registry_github_repo(
    client: &reqwest::Client,
    registry_url: &str,
    module_name: &str,
) -> Result<String, ModuleError> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct RegistryEntry {
        name: String,
        repo: String,
    }

    let resp = client
        .get(registry_url)
        .send()
        .await
        .map_err(|e| ModuleError::op_err("GET registry index failed", e))?;
    if !resp.status().is_success() {
        return Err(ModuleError::OperationError(format!(
            "GET {registry_url} returned HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ModuleError::op_err("Reading registry body", e))?;
    let entries: Vec<RegistryEntry> = serde_json::from_slice(&bytes)
        .map_err(|e| ModuleError::op_err("Registry JSON parse failed", e))?;
    entries
        .into_iter()
        .find(|e| e.name == module_name)
        .map(|e| e.repo)
        .ok_or_else(|| {
            ModuleError::OperationError(format!(
                "Module '{module_name}' not found in registry at {registry_url}"
            ))
        })
}

/// List semver release versions (`tag_name` without leading `v`) from GitHub Releases JSON.
#[cfg(feature = "governance")]
pub fn parse_github_release_versions(json: &[u8]) -> Result<Vec<String>, ModuleError> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
        draft: bool,
        prerelease: bool,
    }

    let releases: Vec<Release> = serde_json::from_slice(json)
        .map_err(|e| ModuleError::op_err("GitHub releases JSON parse failed", e))?;

    let mut versions = Vec::new();
    for release in releases {
        if release.draft || release.prerelease {
            continue;
        }
        let version = release.tag_name.trim().trim_start_matches('v').to_string();
        if crate::module::version_constraint::parse_semver_triple(&version).is_some() {
            versions.push(version);
        }
    }
    Ok(versions)
}

/// List semver release versions (`tag_name` without leading `v`) from GitHub Releases.
#[cfg(feature = "governance")]
pub async fn fetch_github_release_versions(
    client: &reqwest::Client,
    github_repo: &str,
) -> Result<Vec<String>, ModuleError> {
    let url = format!("https://api.github.com/repos/{github_repo}/releases?per_page=100");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| ModuleError::op_err("GET GitHub releases failed", e))?;
    if !resp.status().is_success() {
        return Err(ModuleError::OperationError(format!(
            "GET {url} returned HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ModuleError::op_err("Reading GitHub releases body", e))?;
    parse_github_release_versions(&bytes)
}

/// Resolve the highest GitHub release version matching a constraint (e.g. `0.1.*`).
#[cfg(feature = "governance")]
pub async fn resolve_release_version_for_constraint(
    client: &reqwest::Client,
    github_repo: &str,
    constraint: &str,
) -> Result<String, ModuleError> {
    let constraint = constraint.trim();
    if constraint.is_empty() || constraint == "*" {
        return Err(ModuleError::OperationError(
            "resolve_release_version_for_constraint requires a pinned constraint".to_string(),
        ));
    }
    let versions = fetch_github_release_versions(client, github_repo).await?;
    crate::module::version_constraint::select_highest_matching_version(&versions, constraint)
}

/// Download the release checksums file (`sha256sums.txt` or `SHA256SUMS`) as UTF-8 text.
#[cfg(feature = "governance")]
pub async fn fetch_release_checksums_text(
    client: &reqwest::Client,
    github_repo: &str,
    tag: &str,
) -> Result<String, ModuleError> {
    for cf in CHECKSUM_FILENAMES {
        let url = release_download_url(github_repo, tag, cf);
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ModuleError::op_err("GET checksums URL failed", e))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        if !resp.status().is_success() {
            return Err(ModuleError::OperationError(format!(
                "GET {url} returned HTTP {}",
                resp.status()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ModuleError::op_err("Reading checksums body", e))?;
        return std::str::from_utf8(&bytes)
            .map(|s| s.to_string())
            .map_err(|e| ModuleError::op_err("Checksums file is not UTF-8", e));
    }
    Err(ModuleError::OperationError(format!(
        "No checksums file at GitHub release {tag} for {github_repo} (tried {CHECKSUM_FILENAMES:?})"
    )))
}

/// Expected SHA-256 (hex, lowercase) for this host's release artifact from `sha256sums.txt`.
#[cfg(feature = "governance")]
pub async fn try_fetch_expected_sha_for_native_module(
    client: &reqwest::Client,
    registry_url: &str,
    manifest: &crate::module::registry::manifest::ModuleManifest,
) -> Result<String, ModuleError> {
    let platform = host_platform_key()?;
    let artifact = artifact_name(&manifest.name, platform)?;
    let github_repo = fetch_registry_github_repo(client, registry_url, &manifest.name).await?;
    validate_github_repo(&github_repo)?;
    let tag = release_tag(&manifest.version);
    let checksum_text = fetch_release_checksums_text(client, &github_repo, &tag).await?;
    sha256_from_checksums(&checksum_text, &artifact)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_from_checksums_finds_line() {
        let content = "7216f60f508ff0e82c161e902726deb81cfaba2ee0e10d08c4e0c2e893a9ec2e  blvm-fibre-x86_64-linux\n";
        assert_eq!(
            sha256_from_checksums(content, "blvm-fibre-x86_64-linux").unwrap(),
            "7216f60f508ff0e82c161e902726deb81cfaba2ee0e10d08c4e0c2e893a9ec2e"
        );
    }

    #[test]
    fn sha256_from_checksums_star_prefix() {
        let h = "a".repeat(64);
        let content = format!("{h} *foo-x86_64-linux\n");
        assert_eq!(
            sha256_from_checksums(&content, "foo-x86_64-linux").unwrap(),
            h
        );
    }

    #[test]
    fn release_tag_strips_no_extra_v() {
        assert_eq!(release_tag("0.1.2"), "v0.1.2");
    }

    #[test]
    fn default_module_toml_raw_url_shape() {
        assert_eq!(
            default_module_toml_raw_url("Foo/bar", "main"),
            "https://raw.githubusercontent.com/Foo/bar/main/module.toml"
        );
    }

    #[cfg(feature = "governance")]
    #[test]
    fn parse_github_release_versions_skips_draft_prerelease_and_non_semver() {
        let json = br#"[
          {"tag_name":"v0.1.0","draft":false,"prerelease":false},
          {"tag_name":"v0.2.0","draft":true,"prerelease":false},
          {"tag_name":"v0.3.0-rc1","draft":false,"prerelease":true},
          {"tag_name":"not-a-version","draft":false,"prerelease":false},
          {"tag_name":"v0.1.5","draft":false,"prerelease":false}
        ]"#;
        let versions = parse_github_release_versions(json).unwrap();
        assert_eq!(versions, vec!["0.1.0".to_string(), "0.1.5".to_string()]);
    }

    #[cfg(feature = "governance")]
    #[test]
    fn resolve_release_version_for_constraint_picks_highest_patch() {
        let json = br#"[
          {"tag_name":"v0.1.0","draft":false,"prerelease":false},
          {"tag_name":"v0.1.5","draft":false,"prerelease":false},
          {"tag_name":"v0.2.0","draft":false,"prerelease":false}
        ]"#;
        let versions = parse_github_release_versions(json).unwrap();
        let picked =
            crate::module::version_constraint::select_highest_matching_version(&versions, "0.1.*")
                .unwrap();
        assert_eq!(picked, "0.1.5");
    }
}
