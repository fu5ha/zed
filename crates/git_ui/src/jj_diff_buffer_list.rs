use crate::{
    commit_view::{GitBlob, build_buffer, build_buffer_diff, worktree_id_for_repo_path},
    jj_diff::{Comparison, selected_bookmark_target},
};
use anyhow::{Context as _, Result, bail};
use collections::HashSet;
use git::{jj::JjReadSession, repository::RepoPath};
use gpui::{App, AsyncWindowContext, Entity};
use language::{Buffer, Capability, OffsetRangeExt as _, Point};
use multi_buffer::PathKey;
use project::{
    Project,
    git_store::Repository,
    jj_store::{JjSnapshot, JjStore},
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub(crate) struct JjDiffBufferList {
    project: Entity<Project>,
    repository: Entity<Repository>,
    store: Entity<JjStore>,
    pub scopes: Vec<String>,
    excluded_prefixes: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct ResolvedComparison {
    pub base: String,
    pub head: String,
    pub title: String,
    pub live: bool,
    pub paths: Vec<String>,
    pub session: JjReadSession,
}

pub(crate) struct DiffCandidate {
    pub path: String,
    old: Option<Arc<[u8]>>,
    new: Option<Arc<[u8]>>,
    target_exists: bool,
}

pub(crate) struct LoadedDiff {
    pub key: PathKey,
    pub buffer: Entity<Buffer>,
    pub diff: Entity<buffer_diff::BufferDiff>,
    pub ranges: Vec<std::ops::Range<Point>>,
    pub version: clock::Global,
}

impl JjDiffBufferList {
    pub fn new(
        project: Entity<Project>,
        repository: Entity<Repository>,
        store: Entity<JjStore>,
        cx: &App,
    ) -> Self {
        let root = &repository.read(cx).work_directory_abs_path;
        let mut scopes = Vec::new();
        for worktree in project.read(cx).visible_worktrees(cx) {
            let absolute = worktree.read(cx).abs_path();
            if root.starts_with(absolute.as_ref()) {
                scopes.push(String::new());
            } else if let Ok(relative) = absolute.strip_prefix(root) {
                if let Some(relative) = relative.to_str() {
                    scopes.push(relative.replace('\\', "/"));
                }
            }
        }
        scopes.sort();
        scopes.dedup();
        let mut excluded_prefixes = Vec::new();
        for nested in project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
        {
            if let Ok(relative) = nested.read(cx).work_directory_abs_path.strip_prefix(root) {
                if !relative.as_os_str().is_empty() {
                    if let Some(relative) = relative.to_str() {
                        excluded_prefixes.push(relative.replace('\\', "/"));
                    }
                }
            }
        }
        Self {
            project,
            repository,
            store,
            scopes,
            excluded_prefixes,
        }
    }

    pub fn filter_path(&self, path: &str) -> bool {
        is_content_path(path)
            && self.scopes.iter().any(|scope| path_is_within(path, scope))
            && !self
                .excluded_prefixes
                .iter()
                .any(|prefix| path_is_within(path, prefix))
    }

    pub fn dirty_paths(&self, cx: &App) -> Vec<String> {
        let root = &self.repository.read(cx).work_directory_abs_path;
        self.project
            .read(cx)
            .buffer_store()
            .read(cx)
            .buffers()
            .filter_map(|buffer| {
                let buffer = buffer.read(cx);
                if !buffer.is_dirty() {
                    return None;
                }
                let absolute = buffer.file()?.as_local()?.abs_path(cx);
                let path = absolute
                    .strip_prefix(root)
                    .ok()?
                    .to_str()?
                    .replace('\\', "/");
                self.filter_path(&path).then_some(path)
            })
            .collect()
    }

    pub async fn discover(
        &self,
        comparison: &Comparison,
        snapshot: Arc<JjSnapshot>,
        bookmarks_snapshot: Option<Arc<JjSnapshot>>,
        cx: &mut AsyncWindowContext,
    ) -> Result<ResolvedComparison> {
        let live = !matches!(comparison, Comparison::Change(_));
        if live && snapshot.session.sparse_patterns().await?.trim() != "." {
            bail!("jj live diffs do not yet support sparse workspaces");
        }
        let mut bookmarks_snapshot = bookmarks_snapshot.unwrap_or_else(|| snapshot.clone());
        if matches!(
            comparison,
            Comparison::Bookmark(_) | Comparison::WorkingCopyBookmark(_)
        ) {
            let mut pinned = (*bookmarks_snapshot).clone();
            pinned.bookmarks = Arc::new(snapshot.session.bookmarks().await?);
            bookmarks_snapshot = Arc::new(pinned);
        }
        let bookmarks = bookmarks_snapshot.as_ref();
        let (base, head, title) = match comparison {
            Comparison::Current => {
                if let Some(error) = &snapshot.baseline_error {
                    bail!("{error}");
                }
                (
                    snapshot.base_revision.clone(),
                    snapshot.revision.commit_id.clone(),
                    "jj current change".to_owned(),
                )
            }
            Comparison::Change(commit) => {
                let revision = snapshot.session.revision(commit).await?;
                if revision.parents.len() != 1 {
                    bail!("jj historical diffs do not yet support merged-parent changes");
                }
                (
                    revision
                        .parents
                        .first()
                        .context("missing jj parent")?
                        .clone(),
                    revision.commit_id,
                    format!(
                        "jj change {}",
                        revision.change_id.chars().take(8).collect::<String>()
                    ),
                )
            }
            Comparison::Bookmark(selection) => {
                let (target, name) = selected_bookmark_target(bookmarks, selection)?;
                let ancestors = format!("heads(::{} & ::{})", target, snapshot.revision.commit_id);
                let base = snapshot
                    .session
                    .revision(&ancestors)
                    .await
                    .context("jj bookmark comparison needs exactly one common ancestor")?
                    .commit_id;
                (
                    base,
                    snapshot.revision.commit_id.clone(),
                    format!("jj since {name}"),
                )
            }
            Comparison::WorkingCopyChange(commit) => {
                let revision = snapshot.session.revision(commit).await?;
                let name = revision.change_id.chars().take(8).collect::<String>();
                (
                    revision.commit_id,
                    snapshot.revision.commit_id.clone(),
                    format!("jj {name} → working copy"),
                )
            }
            Comparison::WorkingCopyBookmark(selection) => {
                let (target, name) = selected_bookmark_target(bookmarks, selection)?;
                (
                    target,
                    snapshot.revision.commit_id.clone(),
                    format!("jj {name} → working copy"),
                )
            }
        };
        let changes = self
            .store
            .update(cx, |store, cx| {
                store.comparison_files(
                    snapshot.session.clone(),
                    base.clone(),
                    head.clone(),
                    self.scopes.clone(),
                    cx,
                )
            })
            .await?;
        if changes
            .iter()
            .any(|change| change.source.conflict || change.target.conflict)
        {
            bail!("jj comparison contains conflicted files");
        }
        let mut paths = changes
            .iter()
            .flat_map(|change| {
                [
                    change.path.clone(),
                    change.source.path.clone(),
                    change.target.path.clone(),
                ]
            })
            .filter(|path| self.filter_path(path))
            .collect::<Vec<_>>();
        let mut seen = HashSet::default();
        paths.retain(|path| seen.insert(path.clone()));
        Ok(ResolvedComparison {
            base,
            head,
            title,
            live,
            paths,
            session: snapshot.session.clone(),
        })
    }

    pub async fn candidates(
        &self,
        resolved: &ResolvedComparison,
        mut paths: Vec<String>,
        cx: &mut AsyncWindowContext,
    ) -> Result<Vec<DiffCandidate>> {
        paths.retain(|path| self.filter_path(path));
        let mut seen = HashSet::default();
        paths.retain(|path| seen.insert(path.clone()));
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        // These Git object reads deliberately do not depend on the private operation's lifetime.
        let base = self.store.update(cx, |store, cx| {
            store.comparison_tree_entries(
                resolved.session.clone(),
                resolved.base.clone(),
                paths.clone(),
                cx,
            )
        });
        let head = if resolved.live {
            gpui::Task::ready(Ok(Arc::new(Vec::new())))
        } else {
            self.store.update(cx, |store, cx| {
                store.comparison_tree_entries(
                    resolved.session.clone(),
                    resolved.head.clone(),
                    paths.clone(),
                    cx,
                )
            })
        };
        let (base, head) = futures::try_join!(base, head)?;
        let unsupported = base
            .iter()
            .chain(head.iter())
            .filter(|entry| !entry.is_regular_file() && entry.mode != "040000")
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        paths.retain(|path| {
            !unsupported
                .iter()
                .any(|prefix| path_is_within(path, prefix))
        });
        let allowed = paths.iter().cloned().collect::<HashSet<_>>();
        let base = base
            .iter()
            .filter(|entry| entry.is_regular_file() && allowed.contains(&entry.path))
            .map(|entry| (entry.path.clone(), entry.oid.clone()))
            .collect::<BTreeMap<_, _>>();
        let head = head
            .iter()
            .filter(|entry| entry.is_regular_file() && allowed.contains(&entry.path))
            .map(|entry| (entry.path.clone(), entry.oid.clone()))
            .collect::<BTreeMap<_, _>>();
        let objects = base
            .values()
            .chain(head.values().filter(|_| !resolved.live))
            .map(|oid| (oid.clone(), oid.clone()))
            .collect();
        let blobs = self
            .store
            .update(cx, |store, cx| {
                store.comparison_blob_contents(resolved.session.clone(), objects, cx)
            })
            .await?;
        paths
            .into_iter()
            .map(|path| {
                let contents = |objects: &BTreeMap<String, String>| -> Result<Option<Arc<[u8]>>> {
                    objects
                        .get(&path)
                        .map(|oid| {
                            blobs
                                .get(oid)
                                .cloned()
                                .context("missing jj comparison blob")
                        })
                        .transpose()
                };
                let old = contents(&base)?;
                let new = if resolved.live {
                    None
                } else {
                    contents(&head)?
                };
                let target_exists = if resolved.live {
                    base.contains_key(&path) || resolved.paths.contains(&path)
                } else {
                    head.contains_key(&path)
                };
                Ok(DiffCandidate {
                    path,
                    old,
                    new,
                    target_exists,
                })
            })
            .collect()
    }

    pub async fn load(
        &self,
        resolved: &ResolvedComparison,
        candidate: DiffCandidate,
        cx: &mut AsyncWindowContext,
    ) -> Result<Option<LoadedDiff>> {
        let DiffCandidate {
            path,
            old,
            new,
            target_exists,
        } = candidate;
        let repo_path = RepoPath::new(&path)?;
        let worktree_id = self
            .repository
            .read_with(cx, |repository, cx| {
                worktree_id_for_repo_path(repository, self.project.read(cx), &repo_path, cx)
            })
            .context("jj comparison has no worktree")?;
        let languages = self
            .project
            .read_with(cx, |project, _| project.languages().clone());
        let mut binary = old.as_deref().is_some_and(is_binary);
        let mut deleted = !target_exists;
        let live_buffer = if resolved.live {
            let existing = cx.update(|_, cx| {
                let root = &self.repository.read(cx).work_directory_abs_path;
                self.project
                    .read(cx)
                    .buffer_store()
                    .read(cx)
                    .buffers()
                    .find(|buffer| {
                        let buffer = buffer.read(cx);
                        buffer
                            .file()
                            .and_then(|file| file.as_local())
                            .is_some_and(|file| file.abs_path(cx) == root.join(&path))
                    })
            })?;
            if existing
                .as_ref()
                .is_some_and(|buffer| buffer.read_with(cx, |buffer, _| buffer.is_dirty()))
            {
                deleted = false;
                existing
            } else {
                let root = self.repository.read_with(cx, |repository, _| {
                    repository.work_directory_abs_path.clone()
                });
                let fs = self
                    .project
                    .read_with(cx, |project, _| project.fs().clone());
                let absolute = root.join(&path);
                let mut parent = absolute.parent();
                while let Some(directory) = parent.filter(|directory| *directory != root.as_ref()) {
                    if fs
                        .metadata(directory)
                        .await?
                        .is_some_and(|metadata| metadata.is_symlink)
                    {
                        return Ok(None);
                    }
                    parent = directory.parent();
                }
                if let Some(metadata) = fs.metadata(&absolute).await? {
                    if metadata.is_symlink || metadata.is_dir || metadata.is_fifo {
                        return Ok(None);
                    }
                    if !fs
                        .canonicalize(&absolute)
                        .await?
                        .starts_with(fs.canonicalize(&root).await?)
                    {
                        return Ok(None);
                    }
                    deleted = false;
                    let project_path = self
                        .repository
                        .read_with(cx, |repository, cx| {
                            repository.repo_path_to_project_path(&repo_path, cx)
                        })
                        .context("jj comparison file is outside the opened worktrees")?;
                    if old.is_none()
                        && !target_exists
                        && cx.update(|_, cx| {
                            self.project
                                .read(cx)
                                .worktree_for_id(project_path.worktree_id, cx)
                                .is_some_and(|worktree| {
                                    worktree.read(cx).is_path_ignored(&project_path.path)
                                })
                        })?
                    {
                        return Ok(None);
                    }
                    if let Some(buffer) = existing {
                        match refresh_clean_buffer(&buffer, &self.project, cx).await {
                            Ok(true) => Some(buffer),
                            Ok(false) => bail!("jj comparison file changed while loading: {path}"),
                            Err(error) if format!("{error:#}").contains("Binary") => {
                                binary = true;
                                None
                            }
                            Err(error) => return Err(error),
                        }
                    } else {
                        match self
                            .project
                            .update(cx, |project, cx| project.open_buffer(project_path, cx))
                            .await
                        {
                            Ok(buffer) => Some(buffer),
                            Err(error) if format!("{error:#}").contains("Binary") => {
                                binary = true;
                                None
                            }
                            Err(error) => return Err(error),
                        }
                    }
                } else {
                    deleted = true;
                    None
                }
            }
        } else {
            binary |= new.as_deref().is_some_and(is_binary);
            None
        };
        if old.is_none() && deleted && live_buffer.is_none() {
            return Ok(None);
        }
        let created_or_deleted = old.is_none() || deleted;
        let old_text = if binary {
            None
        } else {
            old.map(|bytes| String::from_utf8(bytes.to_vec()))
                .transpose()?
        };
        let buffer = if let Some(buffer) = live_buffer
            .filter(|buffer| !binary || buffer.read_with(cx, |buffer, _| buffer.is_dirty()))
        {
            buffer
        } else {
            let text = if binary {
                "(binary file not shown)\n".to_owned()
            } else if resolved.live {
                String::new()
            } else {
                new.map(|bytes| String::from_utf8(bytes.to_vec()))
                    .transpose()?
                    .unwrap_or_default()
            };
            let file = Arc::new(GitBlob {
                path: repo_path.clone(),
                worktree_id,
                is_deleted: deleted,
                is_binary: binary,
                display_name: path.clone(),
            });
            let buffer = build_buffer(text, file, &languages, cx).await?;
            buffer.update(cx, |buffer, cx| {
                buffer.set_capability(Capability::ReadOnly, cx)
            });
            buffer
        };
        let version = buffer.read_with(cx, |buffer, _| buffer.version());
        let diff = build_buffer_diff(old_text, &buffer, &languages, cx).await?;
        let ranges = cx.update(|_, cx| {
            let dirty = resolved.live && buffer.read(cx).is_dirty();
            let snapshot = buffer.read(cx).snapshot();
            if binary {
                return vec![Point::zero()..snapshot.max_point()];
            }
            let ranges = diff
                .read(cx)
                .snapshot(cx)
                .hunks(&snapshot)
                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                .collect::<Vec<_>>();
            if ranges.is_empty() && (created_or_deleted || dirty) {
                vec![Point::zero()..snapshot.max_point()]
            } else {
                ranges
            }
        })?;
        Ok(Some(LoadedDiff {
            key: PathKey::with_sort_prefix(0, repo_path.as_ref().clone()),
            buffer,
            diff,
            ranges,
            version,
        }))
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

pub(crate) fn path_is_within(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) fn is_content_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|component| matches!(component, "" | "." | ".." | ".jj" | ".git"))
}

async fn refresh_clean_buffer(
    buffer: &Entity<Buffer>,
    project: &Entity<Project>,
    cx: &mut AsyncWindowContext,
) -> Result<bool> {
    let initial = cx.update(|_, cx| {
        let value = buffer.read(cx);
        if value.is_dirty() {
            return None;
        }
        let file = value.file()?.clone();
        let absolute = file.as_local()?.abs_path(cx);
        Some((
            value.version(),
            file.clone(),
            file.disk_state(),
            value.encoding(),
            value.has_bom(),
            absolute,
        ))
    })?;
    let Some((version, file, disk_state, encoding, has_bom, absolute)) = initial else {
        return Ok(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    };
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    let Some(before) = fs.metadata(&absolute).await? else {
        return Ok(false);
    };
    if before.is_symlink || before.is_dir || before.is_fifo {
        return Ok(false);
    }
    let bytes = cx
        .update(|_, cx| file.as_local().map(|file| file.load_bytes(cx)))?
        .context("jj working-copy buffer lost its local file")?
        .await?;
    let Some(after) = fs.metadata(&absolute).await? else {
        return Ok(false);
    };
    if before.mtime != after.mtime
        || before.len != after.len
        || before.inode != after.inode
        || after.is_symlink
        || after.is_dir
        || after.is_fifo
    {
        return Ok(false);
    }
    if bytes.contains(&0) {
        bail!("Binary files are not supported");
    }
    let (text, decoded_encoding, had_errors) = encoding.decode(&bytes);
    if had_errors
        || decoded_encoding != encoding
        || has_bom != bytes.starts_with(&[0xef, 0xbb, 0xbf])
    {
        return Ok(false);
    }
    let diff = buffer
        .update(cx, |buffer, cx| buffer.diff(text.into_owned(), cx))
        .await;
    buffer.update(cx, |buffer, cx| {
        // Guard the complete read and calculation, not just the diff's captured version.
        if buffer.is_dirty() || buffer.version() != version {
            return Ok(true);
        }
        if buffer.file().is_none_or(|current| {
            !Arc::ptr_eq(current, &file) || current.disk_state() != disk_state
        }) || buffer.encoding() != encoding
            || buffer.has_bom() != has_bom
        {
            return Ok(false);
        }
        buffer.finalize_last_transaction();
        let line_ending = diff.line_ending;
        buffer.apply_diff(diff, cx);
        buffer.finalize_last_transaction();
        buffer.did_reload(buffer.version(), line_ending, Some(after.mtime), cx);
        Ok(true)
    })
}
