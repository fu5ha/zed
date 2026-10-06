use crate::git_store::{
    GitStore, GitStoreEvent, RepositoryEvent, RepositoryId, RepositoryState, StatusEntry,
};
use anyhow::{Context as _, Result, bail};
use buffer_diff::BufferDiff;
use collections::HashMap;
use fs::Fs;
use futures::{FutureExt as _, StreamExt as _, future::Shared};
use git::jj::{JjBookmark, JjFile, JjReadSession, JjRepository, JjRevision};
use git::{
    repository::RepoPath,
    status::{FileStatus, StatusCode},
};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity};
use language::{Buffer, BufferEvent};
use std::{path::PathBuf, sync::Arc, time::Duration};
use text::BufferId;
use util::ResultExt as _;

#[derive(Clone)]
pub struct JjSnapshot {
    pub backend: JjRepository,
    pub session: JjReadSession,
    pub revision: JjRevision,
    pub base_revision: String,
    pub base_files: Arc<HashMap<String, JjFile>>,
    pub current_files: Arc<HashMap<String, JjFile>>,
    pub baseline_error: Option<String>,
    pub bookmarks: Arc<Vec<JjBookmark>>,
    pub bookmark_error: Option<String>,
}

struct LiveDiff {
    generation: u64,
    path: String,
    buffer: WeakEntity<Buffer>,
    diff: WeakEntity<BufferDiff>,
    _subscription: Subscription,
    recalculation: Task<()>,
}

pub struct JjStore {
    pub snapshot: Option<JjSnapshot>,
    pub error: Option<String>,
    pub snapshot_error: Option<String>,
    root: PathBuf,
    repository_id: RepositoryId,
    git_store: WeakEntity<GitStore>,
    repository_state: Shared<Task<Result<RepositoryState, String>>>,
    ready: Shared<Task<Result<JjSnapshot, String>>>,
    refresh_task: Task<()>,
    watcher_task: Task<()>,
    snapshot_task: Shared<Task<Result<(), String>>>,
    snapshot_running: bool,
    snapshot_pending: bool,
    diffs: HashMap<BufferId, LiveDiff>,
    contents: HashMap<(String, String), Option<Arc<str>>>,
    generation: u64,
    loading_diffs:
        HashMap<(BufferId, u64), Shared<Task<Result<Entity<BufferDiff>, Arc<anyhow::Error>>>>>,
}

impl JjStore {
    pub fn set_repository_state(
        &mut self,
        state: Shared<Task<Result<RepositoryState, String>>>,
        cx: &mut Context<Self>,
    ) {
        self.repository_state = state;
        self.refresh(cx);
    }

    pub fn new(
        root: PathBuf,
        repository_id: RepositoryId,
        git_store: WeakEntity<GitStore>,
        repository_state: Shared<Task<Result<RepositoryState, String>>>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            snapshot: None,
            error: None,
            snapshot_error: None,
            root: root.clone(),
            repository_id,
            git_store,
            repository_state,
            ready: Task::ready(Err("jj state is loading".into())).shared(),
            refresh_task: Task::ready(()),
            watcher_task: Task::ready(()),
            snapshot_task: Task::ready(Ok(())).shared(),
            snapshot_running: false,
            snapshot_pending: false,
            diffs: HashMap::default(),
            contents: HashMap::default(),
            generation: 0,
            loading_diffs: HashMap::default(),
        };
        this.refresh(cx);
        this.watcher_task = cx.spawn(async move |this, cx| {
            let metadata = root.join(".jj");
            let (mut events, watcher) = fs.watch(&metadata, Duration::from_millis(200)).await;
            // Linux watches are not recursive. These directories contain operation and workspace heads.
            for directory in ["repo", "repo/op_heads", "working_copy"] {
                watcher.add(&metadata.join(directory)).log_err();
            }
            while events.next().await.is_some() {
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        });
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let state = self.repository_state.clone();
        let root = self.root.clone();
        self.ready = cx.background_spawn(async move {
            let result: Result<JjSnapshot> = async {
                let RepositoryState::Local(state) = state.await.map_err(anyhow::Error::msg)? else {
                    bail!("jj integration currently requires a local repository");
                };
                if !state.backend.is_trusted() {
                    bail!("Trust this workspace to enable jj integration");
                }
                if !state.fs.is_dir(&root.join(".jj/repo")).await {
                    bail!("jj integration currently supports only colocated single-workspace repositories");
                }
                let binary = which::which_in("jj", state.environment.get("PATH"), &root)
                    .context("jj executable not found in the project environment")?;
                let backend = JjRepository::new(root, binary, state.environment.iter().map(|(key, value)| (key.clone(), value.clone())));
                let session = backend.read_session().await?;
                let sparse = session.sparse_patterns().await?.trim() != ".";
                let revision = session.revision("@").await?;
                let (bookmarks, bookmark_error) = match session.bookmarks().await {
                    Ok(bookmarks) => (Arc::new(bookmarks), None),
                    Err(error) => {
                        let error = format!("{error:#}");
                        log::warn!("jj bookmarks: {error}");
                        (Arc::new(Vec::new()), Some(error))
                    }
                };
                let current_files = Arc::new(session.files(&revision.commit_id).await?.into_iter().map(|file| (file.path.clone(), file)).collect());
                let baseline_error = if sparse {
                    Some("jj gutter diffs do not yet support sparse workspaces".into())
                } else if revision.parents.len() != 1 {
                    Some("jj gutter diffs do not yet support merged-parent changes; no Git baseline will be substituted".into())
                } else {
                    None
                };
                let base_revision = revision.parents.first().cloned().unwrap_or_default();
                let base_files = Arc::new(if baseline_error.is_none() {
                    session.files(&base_revision).await?.into_iter().map(|file| (file.path.clone(), file)).collect()
                } else {
                    HashMap::default()
                });
                Ok(JjSnapshot { backend, session, revision, base_revision, base_files, current_files, baseline_error, bookmarks, bookmark_error })
            }.await;
            result.map_err(|error| format!("{error:#}"))
        }).shared();
        let ready = self.ready.clone();
        self.refresh_task = cx.spawn(async move |this, cx| {
            let result = ready.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(snapshot) => {
                        this.snapshot = Some(snapshot);
                        this.error = None;
                    }
                    Err(error) => {
                        log::warn!("jj: {error}");
                        this.snapshot = None;
                        this.error = Some(error);
                    }
                }
                // A marker load may have already consumed this generation's ready session.
                this.diffs
                    .retain(|_, entry| entry.generation == this.generation);
                let base = this
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.base_revision.clone());
                this.contents
                    .retain(|(revision, _), _| Some(revision) == base.as_ref());
                this.git_store
                    .update(cx, |_, cx| {
                        cx.emit(GitStoreEvent::DiffBaseChanged(Some(this.repository_id)));
                        cx.notify();
                    })
                    .log_err();
                cx.notify();
            })
            .log_err();
        });
    }

    pub fn label(&self) -> String {
        match &self.snapshot {
            Some(snapshot) => {
                let label = format!(
                    "jj {}",
                    snapshot
                        .revision
                        .change_id
                        .chars()
                        .take(8)
                        .collect::<String>()
                );
                let bookmarks = self.current_bookmarks();
                if bookmarks.is_empty() {
                    label
                } else {
                    let names = bookmarks
                        .iter()
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                    let remaining = bookmarks.len().saturating_sub(2);
                    if remaining == 0 {
                        format!("{label} · {names}")
                    } else {
                        format!("{label} · {names} +{remaining}")
                    }
                }
            }
            None if self.error.is_some() => "jj unavailable".into(),
            None => "jj loading".into(),
        }
    }

    pub fn current_bookmarks(&self) -> Vec<String> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut names = snapshot
            .bookmarks
            .iter()
            .filter(|bookmark| {
                bookmark.remote.is_none() && bookmark.targets.contains(&snapshot.revision.commit_id)
            })
            .map(|bookmark| {
                if bookmark.conflict {
                    format!("{} (conflicted)", bookmark.name)
                } else {
                    bookmark.name.clone()
                }
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    pub fn bookmark_summary(&self) -> String {
        if self.snapshot.is_none() {
            return match &self.error {
                Some(error) => format!("Bookmarks unavailable: {error}"),
                None => "Loading bookmarks".into(),
            };
        }
        if let Some(error) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.bookmark_error.as_ref())
        {
            return format!("Bookmarks unavailable: {error}");
        }
        let names = self.current_bookmarks();
        if names.is_empty() {
            "No local bookmarks point to this change".into()
        } else {
            format!("Local bookmarks at this change: {}", names.join(", "))
        }
    }

    pub fn snapshot_working_copy(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.request_snapshot_working_copy(cx);
        let task = self.snapshot_task.clone();
        cx.spawn(async move |_, _| task.await.map_err(anyhow::Error::msg))
    }

    pub fn request_snapshot_working_copy(&mut self, cx: &mut Context<Self>) {
        self.snapshot_pending = true;
        if !self.snapshot_running {
            self.snapshot_running = true;
            self.snapshot_task = cx
                .spawn(async move |this, cx| {
                    let mut first_error = None;
                    loop {
                        // Use a fixed window, not a resettable debounce: continuous saves must not starve snapshots.
                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await;
                        let (state, ready) = this
                            .update(cx, |this, _| {
                                this.snapshot_pending = false;
                                (this.repository_state.clone(), this.ready.clone())
                            })
                            .map_err(|error| format!("{error:#}"))?;
                        let result: Result<()> = async {
                            let RepositoryState::Local(state) =
                                state.await.map_err(anyhow::Error::msg)?
                            else {
                                bail!("jj snapshots require a local workspace");
                            };
                            let snapshot = ready.await.map_err(anyhow::Error::msg)?;
                            cx.background_spawn(async move {
                                if !state.backend.is_trusted() {
                                    bail!("Trust this workspace before snapshotting jj");
                                }
                                snapshot.backend.snapshot_working_copy().await
                            })
                            .await
                        }
                        .await;
                        let error = result.err().map(|error| format!("{error:#}"));
                        if let Some(error) = &error {
                            log::warn!("jj working-copy snapshot failed: {error}");
                        }
                        if first_error.is_none() {
                            first_error = error.clone();
                        }
                        let pending = this
                            .update(cx, |this, cx| {
                                this.snapshot_error = error;
                                if this.snapshot_error.is_none() {
                                    this.refresh(cx);
                                }
                                this.notify_statuses(cx);
                                cx.notify();
                                if !this.snapshot_pending {
                                    this.snapshot_running = false;
                                }
                                this.snapshot_pending
                            })
                            .map_err(|error| format!("{error:#}"))?;
                        if !pending {
                            return first_error.map_or(Ok(()), Err);
                        }
                    }
                })
                .shared();
        }
    }

    pub fn open_diff(
        &mut self,
        path: String,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let buffer_id = buffer.read(cx).remote_id();
        if let Some(diff) = self
            .diffs
            .get(&buffer_id)
            .filter(|entry| entry.generation == self.generation && entry.path == path)
            .and_then(|entry| entry.diff.upgrade())
        {
            return Task::ready(Ok(diff));
        }
        let ready = self.ready.clone();
        let generation = self.generation;
        let loading_key = (buffer_id, generation);
        if let Some(task) = self.loading_diffs.get(&loading_key).cloned() {
            return cx
                .spawn(async move |_, _| task.await.map_err(|error| anyhow::anyhow!("{error:#}")));
        }
        let fallback_buffer = buffer.clone();
        let fallback_path = path.clone();
        let task: Task<Result<Entity<BufferDiff>>> = cx.spawn(async move |this, cx| {
            let result = ready.await;
            if this.read_with(cx, |this, _| this.generation)? != generation {
                return this.update(cx, |this, cx| this.open_diff(path, buffer, cx))?.await;
            }
            let snapshot = result.map_err(anyhow::Error::msg)?;
            if let Some(error) = &snapshot.baseline_error {
                bail!("{error}");
            }
            let key = (snapshot.base_revision.clone(), path.clone());
            let diff_path = path.clone();
            let cached = this.read_with(cx, |this, _| this.contents.get(&key).cloned())?;
            let content = match cached {
                Some(content) => content,
                None => {
                    let content = cx.background_spawn(async move {
                        match snapshot.base_files.get(&path) {
                            None => Ok(None),
                            Some(file) if file.file_type != "file" || file.conflict => bail!("jj diff does not support conflicted or non-regular baseline file {path}"),
                            Some(_) => {
                                let bytes = snapshot.session.file_contents(&snapshot.base_revision, &path).await?;
                                let text = crate::git_store::decode_git_text(bytes)?;
                                Ok(Some(Arc::<str>::from(text)))
                            }
                        }
                    }).await?;
                    this.update(cx, |this, _| this.contents.insert(key, content.clone()))?;
                    content
                }
            };
            let diff = cx.new(|cx| BufferDiff::new(&buffer.read(cx).text_snapshot(), buffer.read(cx).language().cloned(), buffer.read(cx).language_registry(), cx));
            let task = diff.update(cx, |diff, cx| diff.set_base_text(content, buffer.read(cx).text_snapshot(), cx));
            task.await;
            if this.read_with(cx, |this, _| this.generation)? != generation {
                return this.update(cx, |this, cx| this.open_diff(diff_path, buffer, cx))?.await;
            }
            this.update(cx, |this, cx| {
                let subscription = cx.subscribe(&buffer, |this, buffer, event, cx| {
                    if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                        this.recalculate(buffer, cx);
                    }
                });
                this.diffs.insert(buffer_id, LiveDiff { generation, path: diff_path, buffer: buffer.downgrade(), diff: diff.downgrade(), _subscription: subscription, recalculation: Task::ready(()) });
                this.notify_statuses(cx);
                this.recalculate(buffer.clone(), cx);
            })?;
            Ok(diff)
        });
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            if this.read_with(cx, |this, _| this.generation)? != generation {
                return this
                    .update(cx, |this, cx| {
                        this.open_diff(fallback_path, fallback_buffer, cx)
                    })?
                    .await;
            }
            match result {
                Ok(diff) => Ok(diff),
                Err(error) => {
                    log::warn!("jj display diff unavailable: {error:#}");
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("{error:#}"));
                        cx.notify();
                        this.notify_statuses(cx);
                    })?;
                    // Replace any previous marker baseline even when the new baseline is unsupported.
                    Ok(cx.new(|cx| {
                        BufferDiff::new_unchanged(
                            &fallback_buffer.read(cx).text_snapshot(),
                            fallback_buffer.read(cx).language().cloned(),
                            fallback_buffer.read(cx).language_registry(),
                            cx,
                        )
                    }))
                }
            }
        });
        let shared = cx
            .spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, _| this.loading_diffs.remove(&loading_key))
                    .log_err();
                result.map_err(Arc::new)
            })
            .shared();
        self.loading_diffs.insert(loading_key, shared.clone());
        cx.spawn(async move |_, _| shared.await.map_err(|error| anyhow::anyhow!("{error:#}")))
    }

    fn recalculate(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let Some(entry) = self.diffs.get_mut(&buffer.read(cx).remote_id()) else {
            return;
        };
        let Some(diff) = entry.diff.upgrade() else {
            return;
        };
        let snapshot = buffer.read(cx).text_snapshot();
        let calculation = diff.read(cx).update_diff(
            snapshot,
            &diff.read(cx).base_text(cx),
            diff.read(cx).base_text_string(cx).map(Arc::from),
            cx,
        );
        entry.recalculation = cx.spawn(async move |this, cx| {
            let update = calculation.await;
            diff.update(cx, |diff, cx| diff.set_snapshot(update, cx));
            this.update(cx, |this, cx| this.notify_statuses(cx))
                .log_err();
        });
        self.diffs
            .retain(|_, entry| entry.buffer.upgrade().is_some());
    }

    pub fn status_for_path(&self, path: &str, cx: &App) -> Option<FileStatus> {
        let entry = self.diffs.values().find(|entry| entry.path == path)?;
        let diff = entry.diff.upgrade()?;
        let diff = diff.read(cx);
        let (added, deleted) = diff.changed_row_counts();
        if !diff.base_text_exists() && !self.snapshot.as_ref()?.current_files.contains_key(path) {
            return Some(FileStatus::Untracked);
        }
        if diff.base_text_exists() && added == 0 && deleted == 0 {
            return None;
        }
        Some(FileStatus::worktree(if diff.base_text_exists() {
            StatusCode::Modified
        } else {
            StatusCode::Added
        }))
    }

    pub fn display_statuses(&self, cx: &App) -> Vec<StatusEntry> {
        let mut entries = self
            .diffs
            .values()
            .filter_map(|entry| {
                Some(StatusEntry {
                    repo_path: RepoPath::new(&entry.path).log_err()?,
                    status: self.status_for_path(&entry.path, cx)?,
                    diff_stat: None,
                    staged_diff_stat: None,
                    unstaged_diff_stat: None,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.repo_path.cmp(&right.repo_path));
        entries.dedup_by(|left, right| left.repo_path == right.repo_path);
        entries
    }

    fn notify_statuses(&self, cx: &mut Context<Self>) {
        self.git_store
            .update(cx, |store, cx| {
                let active = store
                    .active_repository()
                    .is_some_and(|repository| repository.read(cx).id == self.repository_id);
                cx.emit(GitStoreEvent::RepositoryUpdated(
                    self.repository_id,
                    RepositoryEvent::StatusesChanged,
                    active,
                ));
            })
            .log_err();
    }
}
