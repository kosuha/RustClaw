use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::OnceLock;

static ENV_FILE_VALUES: OnceLock<HashMap<String, String>> = OnceLock::new();

pub fn read_env_var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .or_else(|| env_file_values().get(key).cloned())
}

pub fn read_env_file(keys: &[&str]) -> HashMap<String, String> {
    let wanted = keys.iter().copied().collect::<HashSet<_>>();
    env_file_values()
        .iter()
        .filter(|(key, _)| wanted.contains(key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn env_file_values() -> &'static HashMap<String, String> {
    ENV_FILE_VALUES.get_or_init(|| {
        let Ok(current_dir) = std::env::current_dir() else {
            return HashMap::new();
        };
        let path = current_dir.join(".env");
        let Ok(content) = fs::read_to_string(path) else {
            return HashMap::new();
        };
        parse_env_content(&content)
    })
}

fn parse_env_content(content: &str) -> HashMap<String, String> {
    let mut parsed = HashMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some(equals_index) = trimmed.find('=') else {
            continue;
        };
        let key = trimmed[..equals_index].trim();
        if key.is_empty() {
            continue;
        }

        let mut value = trimmed[equals_index + 1..].trim().to_string();
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len().saturating_sub(1)].to_string();
        }
        if value.is_empty() {
            continue;
        }

        parsed.insert(key.to_string(), value);
    }

    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_content_reads_comments_quotes_and_equals() {
        let parsed = parse_env_content(
            r#"
              # comment
              ASSISTANT_NAME=Andy
              OPENAI_API_KEY="sk-openai-123"
              EXTRA='hello=world'
            "#,
        );

        assert_eq!(
            parsed.get("ASSISTANT_NAME").map(String::as_str),
            Some("Andy")
        );
        assert_eq!(
            parsed.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-openai-123")
        );
        assert_eq!(parsed.get("EXTRA").map(String::as_str), Some("hello=world"));
    }

    #[test]
    fn parse_env_content_skips_invalid_or_empty_lines() {
        let parsed = parse_env_content(
            r#"
              INVALID_LINE
              EMPTY=
              =novalue
              VALID=yes
            "#,
        );

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get("VALID").map(String::as_str), Some("yes"));
    }
}
