use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::types::AdditionalMount;

const DEFAULT_ALLOWLIST_PATH: &str = "~/.config/nanoclaw/mount-allowlist.json";
const DEFAULT_BLOCKED_PATTERNS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".gpg",
    ".aws",
    ".azure",
    ".gcloud",
    ".kube",
    ".docker",
    "credentials",
    ".env",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "id_rsa",
    "id_ed25519",
    "private_key",
    ".secret",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedMount {
    pub host_path: String,
    pub container_path: String,
    pub readonly: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct MountAllowlist {
    #[serde(rename = "allowedRoots", alias = "allowed_roots")]
    allowed_roots: Vec<AllowedRoot>,
    #[serde(
        rename = "blockedPatterns",
        alias = "blocked_patterns",
        default = "default_empty_vec"
    )]
    blocked_patterns: Vec<String>,
    #[serde(
        rename = "nonMainReadOnly",
        alias = "non_main_read_only",
        default = "default_true"
    )]
    non_main_read_only: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct AllowedRoot {
    path: String,
    #[serde(
        rename = "allowReadWrite",
        alias = "allow_read_write",
        default = "default_false"
    )]
    allow_read_write: bool,
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_empty_vec() -> Vec<String> {
    Vec::new()
}

pub fn validate_additional_mount(
    mount: &AdditionalMount,
    is_main: bool,
    allowlist_path_override: Option<&Path>,
) -> Result<ValidatedMount, String> {
    let allowlist_path = allowlist_path_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            let raw_path = std::env::var("MOUNT_ALLOWLIST_PATH")
                .ok()
                .unwrap_or_else(|| DEFAULT_ALLOWLIST_PATH.to_string());
            expand_home(&raw_path)
        });
    let allowlist = load_allowlist(&allowlist_path)?;
    validate_mount_against_allowlist(mount, is_main, &allowlist)
}

fn load_allowlist(path: &Path) -> Result<MountAllowlist, String> {
    let content = fs::read_to_string(path).map_err(|err| {
        format!(
            "mount allowlist read failed at {}: {err}",
            path.to_string_lossy()
        )
    })?;
    serde_json::from_str::<MountAllowlist>(&content).map_err(|err| {
        format!(
            "mount allowlist parse failed at {}: {err}",
            path.to_string_lossy()
        )
    })
}

fn validate_mount_against_allowlist(
    mount: &AdditionalMount,
    is_main: bool,
    allowlist: &MountAllowlist,
) -> Result<ValidatedMount, String> {
    let container_path = mount
        .container_path
        .clone()
        .or_else(|| {
            Path::new(&mount.host_path)
                .file_name()
                .and_then(|name| name.to_str().map(ToString::to_string))
        })
        .ok_or_else(|| format!("failed to derive container path for '{}'", mount.host_path))?;

    if !is_valid_container_path(&container_path) {
        return Err(format!("invalid container path '{container_path}'"));
    }

    let host_path = expand_home(&mount.host_path);
    let real_host_path = fs::canonicalize(&host_path).map_err(|err| {
        format!(
            "additional mount host path does not exist '{}': {err}",
            host_path.to_string_lossy()
        )
    })?;
    let real_host_str = real_host_path.to_string_lossy().to_string();

    let blocked_patterns = merged_blocked_patterns(allowlist);
    if let Some(blocked) = first_blocked_match(&real_host_path, &blocked_patterns) {
        return Err(format!(
            "path '{}' matches blocked pattern '{}'",
            real_host_path.to_string_lossy(),
            blocked
        ));
    }

    let Some(allowed_root) = matching_allowed_root(&real_host_path, &allowlist.allowed_roots)
    else {
        return Err(format!(
            "path '{}' is outside of allowed roots",
            real_host_path.to_string_lossy()
        ));
    };

    let requested_read_write = mount.readonly == Some(false);
    let readonly = if requested_read_write {
        if !is_main && allowlist.non_main_read_only {
            true
        } else {
            !allowed_root.allow_read_write
        }
    } else {
        true
    };

    Ok(ValidatedMount {
        host_path: real_host_str,
        container_path: format!("/workspace/extra/{container_path}"),
        readonly,
    })
}

fn merged_blocked_patterns(allowlist: &MountAllowlist) -> Vec<String> {
    let mut blocked = DEFAULT_BLOCKED_PATTERNS
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    for pattern in &allowlist.blocked_patterns {
        if !blocked.iter().any(|existing| existing == pattern) {
            blocked.push(pattern.clone());
        }
    }
    blocked
}

fn first_blocked_match(path: &Path, blocked_patterns: &[String]) -> Option<String> {
    let path_string = path.to_string_lossy().to_string();
    let parts = path
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(ToString::to_string))
        .collect::<Vec<_>>();

    for pattern in blocked_patterns {
        if path_string.contains(pattern) || parts.iter().any(|part| part.contains(pattern)) {
            return Some(pattern.clone());
        }
    }
    None
}

fn matching_allowed_root<'a>(
    path: &Path,
    allowed_roots: &'a [AllowedRoot],
) -> Option<&'a AllowedRoot> {
    for root in allowed_roots {
        let expanded_root = expand_home(&root.path);
        let real_root = match fs::canonicalize(expanded_root) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if path.starts_with(&real_root) {
            return Some(root);
        }
    }
    None
}

fn is_valid_container_path(path: &str) -> bool {
    if path.trim().is_empty() {
        return false;
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return false;
    }
    !path.split('/').any(|part| part == "..")
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(path));
    }
    if let Some(stripped) = path.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(|home| PathBuf::from(home).join(stripped))
            .unwrap_or_else(|_| PathBuf::from(path));
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_allowlist(root: &Path, non_main_read_only: bool) -> MountAllowlist {
        MountAllowlist {
            allowed_roots: vec![AllowedRoot {
                path: root.to_string_lossy().to_string(),
                allow_read_write: true,
            }],
            blocked_patterns: vec![],
            non_main_read_only,
        }
    }

    #[test]
    fn fails_when_host_path_outside_allowlist_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).expect("create root");
        fs::create_dir_all(&outside).expect("create outside");

        let allowlist = sample_allowlist(&root, true);
        let mount = AdditionalMount {
            host_path: outside.to_string_lossy().to_string(),
            container_path: Some("outside".to_string()),
            readonly: Some(false),
        };

        let result = validate_mount_against_allowlist(&mount, true, &allowlist);
        assert!(result.is_err());
    }

    #[test]
    fn blocks_sensitive_paths_by_default_pattern() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("allowed");
        let blocked = root.join(".ssh");
        fs::create_dir_all(&blocked).expect("create blocked path");

        let allowlist = sample_allowlist(&root, true);
        let mount = AdditionalMount {
            host_path: blocked.to_string_lossy().to_string(),
            container_path: Some("ssh".to_string()),
            readonly: Some(true),
        };

        let result = validate_mount_against_allowlist(&mount, true, &allowlist);
        assert!(result.is_err());
    }

    #[test]
    fn non_main_read_write_request_is_forced_read_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("allowed");
        let work = root.join("work");
        fs::create_dir_all(&work).expect("create work");

        let allowlist = sample_allowlist(&root, true);
        let mount = AdditionalMount {
            host_path: work.to_string_lossy().to_string(),
            container_path: Some("work".to_string()),
            readonly: Some(false),
        };

        let validated =
            validate_mount_against_allowlist(&mount, false, &allowlist).expect("validate mount");
        assert!(validated.readonly);
    }

    #[test]
    fn main_group_can_keep_read_write_when_allowed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("allowed");
        let work = root.join("work");
        fs::create_dir_all(&work).expect("create work");

        let allowlist = sample_allowlist(&root, true);
        let mount = AdditionalMount {
            host_path: work.to_string_lossy().to_string(),
            container_path: Some("work".to_string()),
            readonly: Some(false),
        };

        let validated =
            validate_mount_against_allowlist(&mount, true, &allowlist).expect("validate mount");
        assert!(!validated.readonly);
        assert_eq!(validated.container_path, "/workspace/extra/work");
    }

    #[test]
    fn fails_closed_when_allowlist_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mount_root = temp.path().join("allowed");
        fs::create_dir_all(&mount_root).expect("create mount root");
        let mount = AdditionalMount {
            host_path: mount_root.to_string_lossy().to_string(),
            container_path: Some("work".to_string()),
            readonly: Some(true),
        };

        let missing = temp.path().join("missing-allowlist.json");
        let result = validate_additional_mount(&mount, true, Some(&missing));
        assert!(result.is_err());
    }
}
