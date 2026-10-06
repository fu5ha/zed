use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};
use util::command::{Stdio, new_command};

const REVISION_TEMPLATE: &str = r#"'{"commit_id":' ++ json(commit_id) ++ ',"change_id":' ++ json(change_id) ++ ',"description":' ++ json(description) ++ ',"empty":' ++ json(empty) ++ ',"parents":' ++ json(parents.map(|commit| commit.commit_id())) ++ "}" ++ "\n""#;
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
    pub empty: bool,
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

#[derive(Clone, Debug)]
pub struct JjTreeEntry {
    pub path: String,
    pub oid: String,
    pub mode: String,
}

impl JjTreeEntry {
    pub fn is_regular_file(&self) -> bool {
        self.mode == "100644" || self.mode == "100755"
    }
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

    pub async fn private_read_session(&self) -> Result<JjReadSession> {
        // Unlike `util snapshot`, op log prints the private operation's ID.
        let output = self
            .command(
                None,
                &[
                    "--no-integrate-operation",
                    "op",
                    "log",
                    "--no-graph",
                    "-n",
                    "1",
                    "-T",
                    "id",
                ],
            )
            .await?;
        let operation_id = std::str::from_utf8(&output)?.trim().to_owned();
        if operation_id.is_empty() || !operation_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("jj returned an invalid private operation ID");
        }
        Ok(JjReadSession {
            repository: self.clone(),
            operation_id,
        })
    }

    pub async fn new_change(&self) -> Result<()> {
        self.command(None, &["new", "@"]).await?;
        Ok(())
    }

    pub async fn edit_change(&self, commit_id: &str) -> Result<()> {
        validate_object_id(commit_id)?;
        self.command(None, &["edit", commit_id]).await?;
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
    async fn read_tree_entries(
        &self,
        commit_id: &str,
        scopes: &[String],
    ) -> Result<Vec<JjTreeEntry>> {
        validate_object_id(commit_id)?;
        // jj's synthetic root has no Git object. Do not consult the operation:
        // callers may keep using this resolved Git baseline after private-op GC.
        if commit_id.bytes().all(|byte| byte == b'0') {
            return Ok(Vec::new());
        }
        let mut arguments = vec![
            "ls-tree".to_owned(),
            "-t".to_owned(),
            "-z".to_owned(),
            commit_id.to_owned(),
            "--".to_owned(),
        ];
        // Without -t, a matched directory expands into its children even
        // without -r. Exact candidate reads must never do that.
        if !scopes.iter().any(|scope| scope.is_empty() || scope == ".") {
            for scope in scopes {
                validate_repo_path(scope)?;
                arguments.push(format!(":(literal){scope}"));
            }
        }
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let output = self.git_command(&arguments, None).await?;
        output
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let tab = entry
                    .iter()
                    .position(|byte| *byte == b'\t')
                    .context("invalid Git tree entry")?;
                let mut header = std::str::from_utf8(&entry[..tab])?.split(' ');
                let mode = header.next().context("missing Git tree mode")?;
                let kind = header.next().context("missing Git tree kind")?;
                let oid = header.next().context("missing Git tree object")?;
                validate_object_id(oid)?;
                if (mode == "160000" && kind != "commit")
                    || (mode == "040000" && kind != "tree")
                    || (mode != "160000" && mode != "040000" && kind != "blob")
                    || header.next().is_some()
                {
                    bail!("unexpected Git tree object");
                }
                let path = std::str::from_utf8(&entry[tab + 1..])?.to_owned();
                validate_repo_path(&path)?;
                Ok(JjTreeEntry {
                    path,
                    oid: oid.to_owned(),
                    mode: mode.to_owned(),
                })
            })
            .collect()
    }

    pub async fn tree_entries_exact(
        &self,
        commit_id: &str,
        paths: &[String],
    ) -> Result<Vec<JjTreeEntry>> {
        for path in paths {
            validate_repo_path(path)?;
        }
        let mut entries = self.read_tree_entries(commit_id, paths).await?;
        // A file may have become a directory. Do not enumerate its descendants.
        let paths: std::collections::BTreeSet<_> = paths.iter().map(String::as_str).collect();
        entries.retain(|entry| entry.mode != "040000" && paths.contains(entry.path.as_str()));
        Ok(entries)
    }

    pub async fn blob_contents(
        &self,
        objects: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        if objects.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut input = Vec::new();
        for oid in objects.values() {
            validate_object_id(oid)?;
            input.extend_from_slice(oid.as_bytes());
            input.push(b'\n');
        }
        let output = self
            .git_command(&["cat-file".into(), "--batch".into()], Some(input))
            .await?;
        let mut remaining = output.as_slice();
        let mut contents = BTreeMap::new();
        for (path, oid) in objects {
            let newline = remaining
                .iter()
                .position(|byte| *byte == b'\n')
                .context("truncated Git blob header")?;
            let mut header = std::str::from_utf8(&remaining[..newline])?.split(' ');
            if header.next() != Some(oid.as_str()) || header.next() != Some("blob") {
                bail!("unexpected Git blob response");
            }
            let size: usize = header.next().context("missing Git blob size")?.parse()?;
            if header.next().is_some() {
                bail!("invalid Git blob header");
            }
            remaining = &remaining[newline + 1..];
            let blob = remaining.get(..size).context("truncated Git blob")?;
            if remaining.get(size) != Some(&b'\n') {
                bail!("invalid Git blob terminator");
            }
            contents.insert(path.clone(), blob.to_vec());
            let next = size.checked_add(1).context("invalid Git blob size")?;
            remaining = remaining
                .get(next..)
                .context("truncated Git blob terminator")?;
        }
        if !remaining.is_empty() {
            bail!("unexpected trailing Git blob output");
        }
        Ok(contents)
    }

    async fn git_command(&self, arguments: &[String], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
        use futures::{AsyncWriteExt as _, future::try_join};
        let mut command = new_command("git");
        command
            .current_dir(&self.repository.workspace_root)
            .envs(&self.repository.environment)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .args(arguments);
        let mut child = command.spawn().context("failed to start Git tree reader")?;
        let output = if let Some(input) = input {
            let mut stdin = child.stdin.take().context("Git reader stdin missing")?;
            let (_, output) = try_join(
                async move {
                    stdin.write_all(&input).await?;
                    drop(stdin);
                    anyhow::Ok(())
                },
                async move { child.output().await.map_err(anyhow::Error::from) },
            )
            .await?;
            output
        } else {
            child.output().await?
        };
        if !output.status.success() {
            bail!(
                "Git {} failed: {}",
                arguments.first().map(String::as_str).unwrap_or("read"),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

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

    pub async fn recent_revisions(&self) -> Result<Vec<JjRevision>> {
        self.json_lines(&[
            "log",
            "-r",
            "all()",
            "-n",
            "100",
            "--no-graph",
            "-T",
            REVISION_TEMPLATE,
        ])
        .await
    }

    pub async fn nearby_changes(&self) -> Result<Vec<JjRevision>> {
        self.json_lines(&[
            "log",
            "-r",
            "latest(ancestors(@, 3) | descendants(@, 3), 89) | @ | latest(mutable(), 10)",
            "-n",
            "100",
            "--no-graph",
            "-T",
            REVISION_TEMPLATE,
        ])
        .await
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

    pub async fn comparison_files_scoped(
        &self,
        from: &str,
        to: &str,
        scopes: &[String],
    ) -> Result<Vec<JjFileChange>> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let mut arguments = vec![
            "diff".to_owned(),
            "--from".to_owned(),
            from.to_owned(),
            "--to".to_owned(),
            to.to_owned(),
            "-T".to_owned(),
            DIFF_TEMPLATE.to_owned(),
            "--".to_owned(),
        ];
        for scope in scopes {
            if scope.is_empty() || scope == "." {
                arguments.push("all()".to_owned());
            } else {
                validate_repo_path(scope)?;
                arguments.push(format!("root:{}", serde_json::to_string(scope)?));
            }
        }
        let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        self.json_lines(&borrowed).await
    }

    pub async fn bookmarks(&self) -> Result<Vec<JjBookmark>> {
        self.json_lines(&["bookmark", "list", "--all-remotes", "-T", BOOKMARK_TEMPLATE])
            .await
    }

    pub async fn sparse_patterns(&self) -> Result<String> {
        String::from_utf8(self.command(&["sparse", "list"]).await?)
            .context("invalid jj sparse patterns")
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

fn validate_object_id(oid: &str) -> Result<()> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid Git object ID");
    }
    Ok(())
}

fn validate_repo_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains('\0')
        || std::path::Path::new(path).is_absolute()
        || std::path::Path::new(path)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || path.split('/').any(|component| component.is_empty())
    {
        bail!("invalid repository-relative path");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_object_ids() {
        for length in [40, 64] {
            assert!(validate_object_id(&"a".repeat(length)).is_ok());
        }
        for invalid in ["", "@", "root()", "abc123", "--ignore-immutable", "a | @"] {
            assert!(validate_object_id(invalid).is_err());
        }
        assert!(validate_object_id(&"a".repeat(39)).is_err());
        assert!(validate_object_id(&"a".repeat(41)).is_err());
        assert!(validate_object_id(&"z".repeat(40)).is_err());
    }

    #[test]
    fn edit_rejects_revsets_before_running_command() {
        let repository = JjRepository::new(
            PathBuf::from("."),
            PathBuf::from("nonexistent-jj-executable"),
            [],
        );
        for revision in ["@", "all()", "--ignore-immutable", "abc123"] {
            let error = smol::block_on(repository.edit_change(revision))
                .expect_err("non-object IDs must be rejected");
            assert_eq!(error.to_string(), "invalid Git object ID");
        }
    }
}
