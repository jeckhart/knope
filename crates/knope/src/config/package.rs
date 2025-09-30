use std::{ops::Range, path::PathBuf, str::FromStr};

use ::toml::Spanned;
use glob::glob;
use itertools::Itertools;
use knope_config::{Assets, ChangelogSection};
use knope_versioning::{UnknownFile, VersionedFileConfig, jsonc, package, versioned_file::cargo};
use miette::Diagnostic;
use relative_path::{PathExt, RelativePath, RelativePathBuf};
use serde_json::Value;
use thiserror::Error;
use toml_edit::{DocumentMut, TomlError};

use crate::{fs, fs::read_to_string};

/// Represents a single package in `knope.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    pub(crate) name: package::Name,
    /// The files which define the current version of the package.
    pub(crate) versioned_files: Vec<VersionedFileConfig>,
    /// The path to the `CHANGELOG.md` file (if any) to be updated when running [`Step::PrepareRelease`].
    pub(crate) changelog: Option<RelativePathBuf>,
    /// Optional scopes that can be used to filter commits when running [`Step::PrepareRelease`].
    pub(crate) scopes: Option<Vec<String>>,
    /// Extra sections that should be added to the changelog from custom footers in commit messages
    /// or change set types.
    pub(crate) extra_changelog_sections: Vec<ChangelogSection>,
    pub(crate) assets: Option<Assets>,
    pub(crate) ignore_go_major_versioning: bool,
}

impl Package {
    pub(crate) fn find_in_working_dir() -> Result<Vec<Self>, Error> {
        let mut packages = Self::cargo_workspace_members()?;
        packages.extend(Self::npm_workspaces()?);
        packages.extend(Self::deno_workspaces()?);

        if !packages.is_empty() {
            return Ok(packages);
        }

        let default_changelog_path = RelativePathBuf::from("CHANGELOG.md");
        let changelog = default_changelog_path
            .to_path("")
            .exists()
            .then_some(default_changelog_path);

        let versioned_files = VersionedFileConfig::defaults()
            .filter_map(|file_name| {
                let path = file_name.as_path();
                if path.to_path("").exists() {
                    Some(file_name)
                } else {
                    None
                }
            })
            .collect_vec();
        if versioned_files.is_empty() {
            Ok(vec![])
        } else {
            Ok(vec![Self {
                versioned_files,
                changelog,
                ..Self::default()
            }])
        }
    }

    fn cargo_workspace_members() -> Result<Vec<Self>, CargoWorkspaceError> {
        let cargo_toml_path = RelativePath::new("Cargo.toml");
        let Ok(contents) = read_to_string(cargo_toml_path.as_str()) else {
            return Ok(Vec::new());
        };
        let cargo_toml = DocumentMut::from_str(&contents)
            .map_err(|err| CargoWorkspaceError::Toml(err, cargo_toml_path.into()))?;
        let workspace_path = cargo_toml_path
            .parent()
            .ok_or_else(|| CargoWorkspaceError::Parent(cargo_toml_path.into()))?;
        let Some(members) = cargo_toml
            .get("workspace")
            .and_then(|workspace| workspace.as_table()?.get("members")?.as_array())
        else {
            return Ok(Vec::new());
        };

        let cargo_lock_path = workspace_path.join("Cargo.lock");
        let cargo_lock = if cargo_lock_path.to_path("").exists() {
            VersionedFileConfig::new(cargo_lock_path, None).ok()
        } else {
            None
        };

        let members: Vec<WorkspaceMember> = members
            .iter()
            .map(|member_val| {
                let member = member_val.as_str().ok_or(CargoWorkspaceError::Members)?;
                let member_config =
                    VersionedFileConfig::new(workspace_path.join(member).join("Cargo.toml"), None)?;
                let member_contents = read_to_string(member_config.as_path().to_path("."))?;
                let document = DocumentMut::from_str(&member_contents)
                    .map_err(|err| CargoWorkspaceError::Toml(err, member_config.as_path()))?;
                let name = cargo::name_from_document(&document)
                    .ok_or_else(|| CargoWorkspaceError::NoPackageName(member_config.as_path()))?;
                Ok(WorkspaceMember {
                    path: member_config,
                    name: name.to_string(),
                    document,
                })
            })
            .collect::<Result<_, CargoWorkspaceError>>()?;
        Ok(members
            .iter()
            .map(|member| {
                let mut versioned_files: Vec<VersionedFileConfig> = members
                    .iter()
                    .filter_map(|other_member| {
                        if member.name == other_member.name {
                            Some(other_member.path.clone())
                        } else if cargo::contains_dependency(&other_member.document, &member.name) {
                            let mut path = other_member.path.clone();
                            path.dependency = Some(member.name.clone());
                            Some(path)
                        } else {
                            None
                        }
                    })
                    .collect();
                if cargo::contains_dependency(&cargo_toml, &member.name) {
                    versioned_files.extend(
                        VersionedFileConfig::new(
                            cargo_toml_path.to_relative_path_buf(),
                            Some(member.name.clone()),
                        )
                        .ok(),
                    );
                }
                if let Some(cargo_lock) = cargo_lock.clone() {
                    versioned_files.push(cargo_lock);
                }
                Self {
                    name: package::Name::Custom(member.name.clone()),
                    versioned_files,
                    scopes: Some(vec![member.name.clone()]),
                    changelog: None,
                    extra_changelog_sections: vec![],
                    assets: None,
                    ignore_go_major_versioning: false,
                }
            })
            .collect())
    }

    fn npm_workspaces() -> Result<Vec<Self>, NPMWorkspaceError> {
        #[derive(Debug)]
        struct Workspace {
            path: RelativePathBuf,
            value: Value,
        }

        let Some(workspace_patterns) = read_to_string("package.json").ok().and_then(|json| {
            serde_json::Value::from_str(&json)
                .ok()?
                .get("workspaces")?
                .as_array()
                .cloned()
        }) else {
            return Ok(Vec::new());
        };

        let lock_file = PathBuf::from("package-lock.json").exists();

        let mut workspaces = Vec::new();

        for workspace_pattern in workspace_patterns
            .iter()
            .filter_map(|pattern| pattern.as_str())
        {
            let paths = glob(workspace_pattern).map_err(|source| NPMWorkspaceError::Glob {
                pattern: workspace_pattern.to_string(),
                source,
            })?;
            for path in paths {
                let Ok(path) = path else { continue };
                let path = path.join("package.json");
                let Ok(package_json) = read_to_string(&path) else {
                    continue;
                };
                let Ok(json) = serde_json::Value::from_str(&package_json) else {
                    continue;
                };
                let Ok(path) = path.relative_to(".") else {
                    continue;
                };
                workspaces.push(Workspace { path, value: json });
            }
        }

        let mut packages = Vec::with_capacity(workspaces.len());

        for workspace in &workspaces {
            let name = workspace
                .value
                .get("name")
                .and_then(|name| name.as_str())
                .ok_or_else(|| NPMWorkspaceError::NoName {
                    path: workspace.path.clone(),
                })?
                .to_string();
            let mut versioned_files = vec![VersionedFileConfig::new(workspace.path.clone(), None)?];
            if lock_file {
                versioned_files.push(VersionedFileConfig::new(
                    "package-lock.json".into(),
                    Some(name.clone()),
                )?);
            }
            for other_workspace in &workspaces {
                if other_workspace.path == workspace.path {
                    continue;
                }
                if other_workspace
                    .value
                    .get("dependencies")
                    .and_then(|deps| deps.get(&name))
                    .or_else(|| {
                        other_workspace
                            .value
                            .get("devDependencies")
                            .and_then(|deps| deps.get(&name))
                    })
                    .is_some()
                {
                    versioned_files.push(VersionedFileConfig::new(
                        other_workspace.path.clone(),
                        Some(name.clone()),
                    )?);
                }
            }
            packages.push(Package {
                name: package::Name::Custom(name.clone()),
                versioned_files,
                changelog: workspace.path.parent().map(|dir| dir.join("CHANGELOG.md")),
                scopes: Some(vec![name]),
                ..Default::default()
            });
        }

        Ok(packages)
    }

    /// Attempts to read and parse a single deno config file
    fn try_read_deno_config_file(
        file_path: std::path::PathBuf,
    ) -> Result<Option<(RelativePathBuf, Value)>, DenoWorkspaceError> {
        let Ok(contents) = read_to_string(&file_path) else {
            return Ok(None);
        };

        let relative_path = file_path.relative_to(".").map_err(|_| {
            DenoWorkspaceError::UnknownFile(UnknownFile {
                path: RelativePathBuf::from(file_path.to_string_lossy().to_string()),
            })
        })?;

        let json = match serde_json::Value::from_str(&contents) {
            Ok(json) => json,
            Err(_) => {
                let stripped = jsonc::strip_json_comments(&contents);
                serde_json::Value::from_str(&stripped).map_err(|err| DenoWorkspaceError::Json {
                    path: relative_path.clone(),
                    source: err,
                })?
            }
        };

        Ok(Some((relative_path, json)))
    }

    /// Reads deno configuration files in priority order: deno.json, deno.jsonc, package.json
    fn read_deno_config_files(
        base_path: &std::path::Path,
    ) -> Result<Option<(RelativePathBuf, Value)>, DenoWorkspaceError> {
        let file_configs = ["deno.json", "deno.jsonc", "package.json"];

        for filename in file_configs {
            let file_path = base_path.join(filename);
            if let Some(result) = Self::try_read_deno_config_file(file_path)? {
                return Ok(Some(result));
            }
        }

        Ok(None)
    }

    fn deno_workspaces() -> Result<Vec<Self>, DenoWorkspaceError> {
        #[derive(Debug)]
        struct Workspace {
            path: RelativePathBuf,
            value: Value,
        }

        // Check if there's a root deno.json file
        let root_deno_content = read_to_string("deno.json")
            .ok()
            .or_else(|| read_to_string("deno.jsonc").ok());
        let Some(root_deno_content) = root_deno_content else {
            return Ok(Vec::new());
        };

        // Parse the root deno.json, handling JSONC (JSON with comments)
        let root_json = match serde_json::Value::from_str(&root_deno_content) {
            Ok(json) => json,
            Err(_) => {
                // Try to strip comments and parse again (for JSONC)
                let stripped = jsonc::strip_json_comments(&root_deno_content);
                serde_json::Value::from_str(&stripped)
                    .ok()
                    .unwrap_or_default()
            }
        };

        let lock_file = PathBuf::from("deno.lock").exists();
        let mut workspaces = Vec::new();

        // Check if it has workspace patterns
        if let Some(workspace_patterns) = root_json.get("workspace").and_then(|w| w.as_array()) {
            // This is a workspace-based project - process the patterns
            for workspace_pattern in workspace_patterns
                .iter()
                .filter_map(|pattern| pattern.as_str())
            {
                let paths = glob(workspace_pattern).map_err(|source| DenoWorkspaceError::Glob {
                    pattern: workspace_pattern.to_string(),
                    source,
                })?;
                for path in paths {
                    let Ok(path) = path else { continue };
                    // Only process directories, not files
                    if !path.is_dir() {
                        continue;
                    }
                    if let Some((relative_path, json)) = Self::read_deno_config_files(&path)? {
                        workspaces.push(Workspace {
                            path: relative_path,
                            value: json,
                        });
                    }
                }
            }
        } else {
            // No workspace field - return empty (no deno packages to version)
            return Ok(Vec::new());
        }

        let mut packages = Vec::with_capacity(workspaces.len());

        for workspace in &workspaces {
            // Only process workspaces that have both name and version
            let Some(name) = workspace.value.get("name").and_then(|name| name.as_str()) else {
                continue;
            };

            let has_version = workspace
                .value
                .get("version")
                .and_then(|v| v.as_str())
                .is_some();

            if !has_version {
                continue;
            }

            let name = name.to_string();

            let mut versioned_files = vec![VersionedFileConfig::new(workspace.path.clone(), None)?];

            if lock_file {
                versioned_files.push(VersionedFileConfig::new(
                    "deno.lock".into(),
                    Some(name.clone()),
                )?);
            }

            // Check for dependencies between deno packages
            for other_workspace in &workspaces {
                if other_workspace.path == workspace.path {
                    continue;
                }
                if other_workspace
                    .value
                    .get("imports")
                    .and_then(|deps| deps.get(&name))
                    .is_some()
                {
                    versioned_files.push(VersionedFileConfig::new(
                        other_workspace.path.clone(),
                        Some(name.clone()),
                    )?);
                }
            }

            packages.push(Package {
                name: package::Name::Custom(name.clone()),
                versioned_files,
                changelog: workspace.path.parent().map(|dir| dir.join("CHANGELOG.md")),
                scopes: Some(vec![name]),
                ..Default::default()
            });
        }

        Ok(packages)
    }

    pub(crate) fn from_toml(
        name: package::Name,
        package: knope_config::Package,
        source_code: &str,
    ) -> Result<Self, VersionedFileError> {
        let knope_config::Package {
            versioned_files,
            changelog,
            scopes,
            extra_changelog_sections,
            assets,
            ignore_go_major_versioning,
        } = package;
        let versioned_files = versioned_files
            .into_iter()
            .map(|spanned| {
                let span = spanned.span();
                VersionedFileConfig::try_from(spanned.into_inner())
                    .map_err(|source| VersionedFileError::UnknownFile {
                        source,
                        span: span.clone(),
                        source_code: source_code.to_string(),
                    })
                    .and_then(|path| {
                        let pathbuf = path.to_pathbuf();
                        if pathbuf.exists() {
                            Ok(path)
                        } else {
                            Err(VersionedFileError::Missing {
                                path: pathbuf,
                                span,
                                source_code: source_code.to_string(),
                            })
                        }
                    })
            })
            .try_collect()?;
        Ok(Self {
            name,
            versioned_files,
            changelog,
            scopes,
            extra_changelog_sections,
            assets,
            ignore_go_major_versioning,
        })
    }
}

impl From<Package> for knope_config::Package {
    fn from(package: Package) -> Self {
        Self {
            versioned_files: package
                .versioned_files
                .into_iter()
                .map(|it| Spanned::new(0..0, knope_config::VersionedFile::from(it)))
                .collect(),
            changelog: package.changelog,
            scopes: package.scopes,
            extra_changelog_sections: package.extra_changelog_sections,
            assets: package.assets,
            ignore_go_major_versioning: package.ignore_go_major_versioning,
        }
    }
}

#[derive(Debug)]
struct WorkspaceMember {
    path: VersionedFileConfig,
    name: String,
    document: DocumentMut,
}

#[derive(Debug, Diagnostic, Error)]
pub enum VersionedFileError {
    #[error("Problem with versioned file")]
    #[diagnostic()]
    UnknownFile {
        #[diagnostic_source]
        source: UnknownFile,
        #[source_code]
        source_code: String,
        #[label("Declared here")]
        span: Range<usize>,
    },
    #[error("File {path} does not exist")]
    #[diagnostic(
        code(config::missing_versioned_file),
        help("Make sure the file exists and is accessible.")
    )]
    Missing {
        path: PathBuf,
        #[source_code]
        source_code: String,
        #[label("Declared here")]
        span: Range<usize>,
    },
}

#[derive(Debug, Diagnostic, thiserror::Error)]
pub(crate) enum CargoWorkspaceError {
    #[error("Could not find a package.name in {0}")]
    #[diagnostic(code(workspace::no_package_name))]
    NoPackageName(RelativePathBuf),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Fs(#[from] fs::Error),
    #[error("Could not parse TOML in {1}: {0}")]
    #[diagnostic(code(workspace::toml))]
    Toml(TomlError, RelativePathBuf),
    #[error("Could not get parent directory of Cargo.toml file: {0}")]
    #[diagnostic(code(workspace::parent))]
    Parent(RelativePathBuf),
    #[error("The Cargo workspace members array should contain only strings")]
    #[diagnostic(code(workspace::members))]
    Members,
    #[error(transparent)]
    #[diagnostic(transparent)]
    UnknownFile(#[from] UnknownFile),
}

#[derive(Debug, Diagnostic, thiserror::Error)]
pub(crate) enum NPMWorkspaceError {
    #[error("Could not process workspaces glob pattern {pattern} in package.json: {source}")]
    #[diagnostic(code(workspaces::npm_glob))]
    Glob {
        pattern: String,
        source: glob::PatternError,
    },
    #[error("Could not find a name in {path}")]
    #[diagnostic(code(workspaces::npm_no_name))]
    NoName { path: RelativePathBuf },
    #[error(transparent)]
    #[diagnostic(transparent)]
    UnknownFile(#[from] UnknownFile),
}

#[derive(Debug, Diagnostic, Error)]
pub(crate) enum DenoWorkspaceError {
    #[error("Could not process workspaces glob pattern {pattern} in deno.json: {source}")]
    #[diagnostic(code(workspaces::deno_glob))]
    Glob {
        pattern: String,
        source: glob::PatternError,
    },
    #[error("Could not parse JSON in {path}: {source}")]
    #[diagnostic(code(workspaces::deno_json))]
    Json {
        path: RelativePathBuf,
        source: serde_json::Error,
    },
    #[error(transparent)]
    #[diagnostic(transparent)]
    UnknownFile(#[from] UnknownFile),
}

#[derive(Debug, Diagnostic, Error)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    CargoWorkspace(#[from] CargoWorkspaceError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    NPMWorkspace(#[from] NPMWorkspaceError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    DenoWorkspace(#[from] DenoWorkspaceError),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;
    use tempfile::tempdir_in;

    use super::Package;

    #[test]
    fn try_read_deno_config_file_supports_jsonc() {
        let temp_dir = tempdir_in(".").unwrap();
        let file_path = temp_dir.path().join("deno.jsonc");
        fs::write(
            &file_path,
            "{\n  // comment\n  \"name\": \"@scope/package\",\n  \"version\": \"1.0.0\"\n}\n",
        )
        .unwrap();

        let relative_path = file_path
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap()
            .to_path_buf();

        let result = Package::try_read_deno_config_file(relative_path).unwrap();
        let (path, json) = result.expect("file should parse");

        assert!(path.as_str().ends_with("deno.jsonc"));
        assert_eq!(
            json.get("name"),
            Some(&Value::String("@scope/package".into()))
        );
    }
}
