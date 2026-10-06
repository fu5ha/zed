use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};
use util::command::{Stdio, new_command};

const REVISION_TEMPLATE: &str = r#"'{"commit_id":' ++ json(commit_id) ++ ',"change_id":' ++ json(change_id) ++ ',"description":' ++ json(description) ++ ',"parents":' ++ json(parents.map(|commit| commit.commit_id())) ++ "}" ++ "\n""#;
const FILE_TEMPLATE: &str = r#"'{"path":' ++ json(path) ++ ',"file_type":' ++ json(file_type) ++ ',"conflict":' ++ json(conflict) ++ "}" ++ "\n""#;
const DIFF_TEMPLATE: &str = r#"'{"path":' ++ json(path) ++ ',"status":' ++ json(status) ++ ',"source":' ++ '{"path":' ++ json(source.path()) ++ ',"file_type":' ++ json(source.file_type()) ++ ',"conflict":' ++ json(source.conflict()) ++ '},"target":' ++ '{"path":' ++ json(target.path()) ++ ',"file_type":' ++ json(target.file_type()) ++ ',"conflict":' ++ json(target.conflict()) ++ "}}" ++ "\n""#;
const BOOKMARK_TEMPLATE: &str = r#"'{"name":' ++ json(name) ++ ',"remote":' ++ json(remote) ++ ',"conflict":' ++ json(conflict) ++ ',"targets":' ++ json(added_targets.map(|commit| commit.commit_id())) ++ "}" ++ "\n""#;

#[derive(Clone, Debug)]
pub struct JjRepository {
    pub workspace_root: PathBuf,
    pub binary_path: PathBuf,
    environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct JjReadSession {
    repository: JjRepository,
    operation_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct JjRevision {
    pub commit_id: String,
    pub change_id: String,
    pub description: String,
    pub parents: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct JjFile {
    pub path: String,
    pub file_type: String,
    pub conflict: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct JjFileChange {
    pub path: String,
    pub status: String,
    pub source: JjFile,
    pub target: JjFile,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct JjBookmark {
    pub name: String,
    pub remote: Option<String>,
    pub conflict: bool,
    pub targets: Vec<String>,
}

impl JjRepository {
    pub fn new(
        workspace_root: PathBuf,
        binary_path: PathBuf,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            workspace_root,
            binary_path,
            environment: environment.into_iter().collect(),
        }
    }

    pub async fn read_session(&self) -> Result<JjReadSession> {
        let version = self.command(None, &["--version"]).await?;
        let version = std::str::from_utf8(&version)?.trim();
        let supported = version
            .strip_prefix("jj ")
            .and_then(|version| version.split('-').next())
            .map(|version| version.split('.').collect::<Vec<_>>())
            .is_some_and(|parts| {
                parts.first() == Some(&"0")
                    && parts
                        .get(1)
                        .and_then(|minor| minor.parse::<u32>().ok())
                        .is_some_and(|minor| minor >= 41)
            });
        if !supported {
            bail!("jj integration requires jj 0.41+ in the 0.x series; found {version}");
        }

        // Resolving @ explicitly prevents a read from reconciling divergent operations.
        let operation_id = self
            .command(
                Some("@"),
                &["op", "log", "--no-graph", "-n", "1", "-T", "id"],
            )
            .await?;
        let operation_id = std::str::from_utf8(&operation_id)?.trim().to_owned();
        if operation_id.is_empty() || !operation_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("jj returned an invalid operation ID");
        }
        Ok(JjReadSession {
            repository: self.clone(),
            operation_id,
        })
    }

    pub async fn snapshot_working_copy(&self) -> Result<()> {
        // This is deliberately separate from the read API: status snapshots but does not save buffers.
        self.command(None, &["status"]).await?;
        Ok(())
    }

    async fn command(&self, operation: Option<&str>, arguments: &[&str]) -> Result<Vec<u8>> {
        let mut command = new_command(&self.binary_path);
        command
            .current_dir(&self.workspace_root)
            .envs(&self.environment)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .args(["--no-pager", "--color=never"]);
        if let Some(operation) = operation {
            command
                .arg("--ignore-working-copy")
                .arg("--at-op")
                .arg(operation);
        }
        let output = command
            .args(arguments)
            .output()
            .await
            .with_context(|| format!("failed to run {}", self.binary_path.display()))?;
        if !output.status.success() {
            bail!(
                "jj {} failed: {}",
                arguments.first().copied().unwrap_or("command"),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if !output.stderr.is_empty() {
            log::warn!("jj: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(output.stdout)
    }
}

impl JjReadSession {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub async fn revision(&self, revision: &str) -> Result<JjRevision> {
        let revisions: Vec<JjRevision> = self
            .json_lines(&["log", "-r", revision, "--no-graph", "-T", REVISION_TEMPLATE])
            .await?;
        if revisions.len() != 1 {
            bail!(
                "expected one jj revision for {revision}, found {}",
                revisions.len()
            );
        }
        revisions.into_iter().next().context("missing jj revision")
    }

    pub async fn files(&self, revision: &str) -> Result<Vec<JjFile>> {
        self.json_lines(&["file", "list", "-r", revision, "-T", FILE_TEMPLATE])
            .await
    }

    pub async fn change_files(&self, revision: &str) -> Result<Vec<JjFileChange>> {
        self.json_lines(&["diff", "-r", revision, "-T", DIFF_TEMPLATE])
            .await
    }

    pub async fn comparison_files(&self, from: &str, to: &str) -> Result<Vec<JjFileChange>> {
        self.json_lines(&["diff", "--from", from, "--to", to, "-T", DIFF_TEMPLATE])
            .await
    }

    pub async fn bookmarks(&self) -> Result<Vec<JjBookmark>> {
        self.json_lines(&["bookmark", "list", "--all-remotes", "-T", BOOKMARK_TEMPLATE])
            .await
    }

    pub async fn file_contents(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        let fileset = format!("root-file:{}", serde_json::to_string(path)?);
        self.command(&["file", "show", "-r", revision, "-T", "", &fileset])
            .await
    }

    async fn command(&self, arguments: &[&str]) -> Result<Vec<u8>> {
        self.repository
            .command(Some(&self.operation_id), arguments)
            .await
    }

    async fn json_lines<T: serde::de::DeserializeOwned>(
        &self,
        arguments: &[&str],
    ) -> Result<Vec<T>> {
        let output = self.command(arguments).await?;
        std::str::from_utf8(&output)?
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).context("invalid jj JSON response"))
            .collect()
    }
}
