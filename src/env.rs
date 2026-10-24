use std::path::PathBuf;

use anyhow::{bail, Result};

const KEYS: [&str; 3] = ["BILLY_KEY", "billy_key", "BILLY_ACCESS_TOKEN"];

/// The Billy access token, from the environment or the nearest `.env` file
/// in the current directory or one of its parents.
pub fn token() -> Result<String> {
    for key in KEYS {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                return Ok(value.trim().to_string());
            }
        }
    }
    match find_dotenv() {
        Some((_, token)) => Ok(token),
        None => bail!("no Billy token: set BILLY_KEY or put billy_key=... in a .env file"),
    }
}

/// Where `apply` logs every change: BILLY_AUDIT_LOG, else next to the `.env`
/// holding the token, else the current directory.
pub fn audit_path() -> PathBuf {
    if let Ok(path) = std::env::var("BILLY_AUDIT_LOG") {
        if !path.trim().is_empty() {
            return path.into();
        }
    }
    let dir = find_dotenv()
        .map(|(dir, _)| dir)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    dir.join("billy-audit.ndjson")
}

/// The nearest `.env` with a Billy token: its directory and the token.
fn find_dotenv() -> Option<(PathBuf, String)> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join(".env")) {
            if let Some(token) = from_dotenv(&text) {
                return Some((dir, token));
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn from_dotenv(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        (KEYS.contains(&key.trim()) && !value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quoted_key_from_dotenv() {
        let text = "# comment\nOTHER=1\nbilly_key = \"abc123\"\n";
        assert_eq!(from_dotenv(text).as_deref(), Some("abc123"));
        assert_eq!(from_dotenv("OTHER=1"), None);
    }
}
