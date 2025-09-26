use std::fmt::Write;

#[cfg(feature = "miette")]
use miette::Diagnostic;
use relative_path::RelativePathBuf;
use serde::Deserialize;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{action::Action, semver::Version};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DenoJson {
    path: RelativePathBuf,
    raw: String,
    parsed: Json,
    diff: Option<String>,
}

impl DenoJson {
    pub(crate) fn new(path: RelativePathBuf, content: String) -> Result<Self, Error> {
        // Try to parse as-is first (for standard JSON)
        let parsed = match serde_json::from_str(&content) {
            Ok(parsed) => parsed,
            Err(_) => {
                // If that fails, try to strip comments and parse again (for JSONC)
                let stripped = Self::strip_json_comments(&content);
                serde_json::from_str(&stripped).map_err(|err| Error::Deserialize {
                    path: path.clone(),
                    source: err,
                })?
            }
        };

        Ok(DenoJson {
            path,
            raw: content,
            parsed,
            diff: None,
        })
    }

    /// Strip JSON comments (// and /* */) to make JSONC parseable as JSON
    fn strip_json_comments(content: &str) -> String {
        let mut result = String::new();
        let mut chars = content.chars().peekable();
        let mut in_string = false;
        let mut escaped = false;

        while let Some(ch) = chars.next() {
            match ch {
                '"' if !escaped => {
                    in_string = !in_string;
                    result.push(ch);
                }
                '\\' if in_string => {
                    escaped = !escaped;
                    result.push(ch);
                }
                '/' if !in_string && !escaped => {
                    if let Some(&next_ch) = chars.peek() {
                        if next_ch == '/' {
                            // Skip line comment
                            chars.next(); // consume the second '/'
                            while let Some(ch) = chars.next() {
                                if ch == '\n' {
                                    result.push(ch);
                                    break;
                                }
                            }
                        } else if next_ch == '*' {
                            // Skip block comment
                            chars.next(); // consume the '*'
                            let mut found_end = false;
                            while let Some(ch) = chars.next() {
                                if ch == '*' {
                                    if let Some(&'/') = chars.peek() {
                                        chars.next(); // consume the '/'
                                        found_end = true;
                                        break;
                                    }
                                }
                            }
                            if !found_end {
                                // Unclosed comment, but we'll let JSON parser handle the error
                            }
                        } else {
                            result.push(ch);
                        }
                    } else {
                        result.push(ch);
                    }
                }
                _ => {
                    result.push(ch);
                    if ch != '\\' {
                        escaped = false;
                    }
                }
            }
        }

        result
    }

    pub(crate) fn get_version(&self) -> Option<&Version> {
        self.parsed.version.as_ref()
    }

    pub(crate) fn get_path(&self) -> &RelativePathBuf {
        &self.path
    }

    pub(crate) fn set_version(
        mut self,
        new_version: &Version,
        dependency: Option<&str>,
    ) -> serde_json::Result<Self> {
        let mut json = serde_json::from_str::<Map<String, Value>>(&self.raw)?;
        let diff = self.diff.get_or_insert_default();
        if !diff.is_empty() {
            diff.push_str(", ");
        }

        if let Some(dependency) = dependency {
            if let Some(imports) = json
                .get_mut("imports")
                .and_then(|deps| deps.as_object_mut())
            {
                if let Some(version) = imports.get_mut(dependency) {
                    let new_version_string = format!("jsr:{dependency}@^{new_version}");
                    *version = Value::String(new_version_string);
                    write!(diff, "imports.{dependency} = ^{new_version}").unwrap();
                }
            }
        } else {
            json.insert(
                "version".to_string(),
                Value::String(new_version.to_string()),
            );
            write!(diff, "version = {new_version}").unwrap();
        }

        self.raw = serde_json::to_string_pretty(&json)?;
        self.parsed.version = Some(new_version.clone());
        Ok(self)
    }
}

impl DenoJson {
    pub(super) fn write(self) -> Option<Action> {
        self.diff.map(|diff| Action::WriteToFile {
            path: self.path,
            content: self.raw,
            diff,
        })
    }
}

#[derive(Debug, Error)]
#[cfg_attr(feature = "miette", derive(Diagnostic))]
pub enum Error {
    #[error("Could not deserialize {path}")]
    #[cfg_attr(
        feature = "miette",
        diagnostic(
            code(knope_versioning::versioned_file::deno_json::deserialize),
            help("Make sure the file is valid JSON")
        )
    )]
    Deserialize {
        path: RelativePathBuf,
        source: serde_json::Error,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct Json {
    version: Option<Version>,
    #[serde(flatten)]
    other: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_deno_json_with_version() {
        let content = r#"{"name": "@scope/package", "version": "1.0.0"}"#;
        let deno_json = DenoJson::new("deno.json".into(), content.to_string()).unwrap();
        assert_eq!(
            deno_json.get_version(),
            Some(&Version::from_str("1.0.0").unwrap())
        );
    }

    #[test]
    fn test_deno_json_without_version() {
        let content = r#"{"name": "@scope/package", "tasks": {"dev": "deno run main.ts"}}"#;
        let deno_json = DenoJson::new("deno.json".into(), content.to_string()).unwrap();
        assert_eq!(deno_json.get_version(), None);
    }

    #[test]
    fn test_set_version() {
        let content = r#"{"name": "@scope/package", "version": "1.0.0"}"#;
        let deno_json = DenoJson::new("deno.json".into(), content.to_string()).unwrap();
        let new_version = Version::from_str("1.1.0").unwrap();
        let updated = deno_json.set_version(&new_version, None).unwrap();
        assert_eq!(updated.get_version(), Some(&new_version));
    }

    #[test]
    fn test_set_version_on_file_without_version() {
        let content = r#"{"name": "@scope/package", "tasks": {"dev": "deno run main.ts"}}"#;
        let deno_json = DenoJson::new("deno.json".into(), content.to_string()).unwrap();
        let new_version = Version::from_str("1.0.0").unwrap();
        let updated = deno_json.set_version(&new_version, None).unwrap();
        assert_eq!(updated.get_version(), Some(&new_version));
    }
}
