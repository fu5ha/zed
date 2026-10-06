use crate::git_store::{
    GitStore, GitStoreEvent, RepositoryEvent, RepositoryId, RepositoryState, StatusEntry,
};
use anyhow::{Context as _, Result, bail};
use buffer_diff::BufferDiff;
use collections::HashMap;
use fs::Fs;
use futures::{FutureExt as _, StreamExt as _, future::Shared, lock::Mutex};
use git::jj::{
    JjBookmark, JjFile, JjFileChange, JjReadSession, JjRepository, JjRevision, JjTreeEntry,
};
use git::{
    repository::RepoPath,
    status::{FileStatus, StatusCode, UnmergedStatus, UnmergedStatusCode},
};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity};
use language::{Buffer, BufferEvent};
use std::{
    collections::BTreeMap, future::Future, hash::Hash, path::PathBuf, sync::Arc, time::Duration,
};
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
    pub nearby_changes: Arc<Vec<JjRevision>>,
    pub change_error: Option<String>,
    pub statuses: Arc<HashMap<String, FileStatus>>,
}

struct LiveDiff {
    generation: u64,
    path: String,
    buffer: WeakEntity<Buffer>,
    diff: WeakEntity<BufferDiff>,
    _subscription: Subscription,
    recalculation: Task<()>,
}

enum JjMutation {
    Snapshot,
    New,
    Edit(String),
}

type ComparisonBlobs = BTreeMap<String, Arc<[u8]>>;
type BlobRead = Shared<Task<Result<Arc<ComparisonBlobs>, String>>>;
const COMPARISON_BLOB_CACHE_BYTES: usize = 32 * 1024 * 1024;
const COMPARISON_BLOB_CACHE_ENTRIES: usize = 4096;
const COMPARISON_LIST_CACHE_REQUESTS: usize = 32;
const COMPARISON_LIST_CACHE_ENTRIES: usize = 4096;

type ComparisonFilesKey = (String, String, Vec<String>);
type ComparisonTreeKey = (String, Vec<String>);
type ComparisonRead<T> = Shared<Task<Result<Arc<Vec<T>>, String>>>;

struct ComparisonReadCache<K, T: 'static> {
    contents: HashMap<K, Arc<Vec<T>>>,
    pending: HashMap<K, ComparisonRead<T>>,
    entries: usize,
}

impl<K, T: 'static> Default for ComparisonReadCache<K, T> {
    fn default() -> Self {
        Self {
            contents: HashMap::default(),
            pending: HashMap::default(),
            entries: 0,
        }
    }
}

pub struct JjStore {
    pub snapshot: Option<JjSnapshot>,
    pub error: Option<String>,
    pub snapshot_error: Option<String>,
    root: PathBuf,
    repository_id: RepositoryId,
    git_store: WeakEntity<GitStore>,
    repository_state: Shared<Task<Result<RepositoryState, String>>>,
    repository_state_generation: u64,
    ready: Shared<Task<Result<JjSnapshot, String>>>,
    refresh_task: Task<()>,
    watcher_task: Task<()>,
    snapshot_task: Shared<Task<Result<(), String>>>,
    snapshot_running: bool,
    snapshot_pending: bool,
    mutation_lock: Arc<Mutex<()>>,
    comparison_blobs: HashMap<String, Arc<[u8]>>,
    comparison_blob_bytes: usize,
    loading_comparison_blobs: HashMap<String, BlobRead>,
    comparison_files: ComparisonReadCache<ComparisonFilesKey, JjFileChange>,
    comparison_trees: ComparisonReadCache<ComparisonTreeKey, JjTreeEntry>,
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
        self.repository_state_generation += 1;
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
            repository_state_generation: 0,
            ready: Task::ready(Err("jj state is loading".into())).shared(),
            refresh_task: Task::ready(()),
            watcher_task: Task::ready(()),
            snapshot_task: Task::ready(Ok(())).shared(),
            snapshot_running: false,
            snapshot_pending: false,
            mutation_lock: Arc::new(Mutex::new(())),
            comparison_blobs: HashMap::default(),
            comparison_blob_bytes: 0,
            loading_comparison_blobs: HashMap::default(),
            comparison_files: ComparisonReadCache::default(),
            comparison_trees: ComparisonReadCache::default(),
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
                let statuses = Arc::new(change_statuses(
                    session.change_files(&revision.commit_id).await?
                )?);
                let (bookmarks, bookmark_error) = match session.bookmarks().await {
                    Ok(bookmarks) => (Arc::new(bookmarks), None),
                    Err(error) => {
                        let error = format!("{error:#}");
                        log::warn!("jj bookmarks: {error}");
                        (Arc::new(Vec::new()), Some(error))
                    }
                };
                let (nearby_changes, change_error) = match session.nearby_changes().await {
                    Ok(changes) => (Arc::new(changes), None),
                    Err(error) => {
                        let error = format!("{error:#}");
                        log::warn!("jj nearby changes: {error}");
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
                Ok(JjSnapshot { backend, session, revision, base_revision, base_files, current_files, baseline_error, bookmarks, bookmark_error, nearby_changes, change_error, statuses })
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
                this.notify_statuses(cx);
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

    pub fn private_snapshot(&mut self, cx: &mut Context<Self>) -> Task<Result<Arc<JjSnapshot>>> {
        let saved_snapshot = self.snapshot_task.clone();
        let mutation_lock = self.mutation_lock.clone();
        cx.spawn(async move |this, cx| {
            // A save may still be inside the coalescing window, before it owns
            // the mutation lock. Opening immediately after saving must wait.
            saved_snapshot.await.map_err(anyhow::Error::msg)?;
            let _guard = mutation_lock.lock().await;
            loop {
                let (state, generation, root) = this.update(cx, |this, _| {
                    (
                        this.repository_state.clone(),
                        this.repository_state_generation,
                        this.root.clone(),
                    )
                })?;
                let state = state.await.map_err(anyhow::Error::msg)?;
                if !this.update(cx, |this, _| this.repository_state_generation == generation)? {
                    continue;
                }
                let RepositoryState::Local(state) = state else {
                    bail!("jj private snapshots require a local workspace");
                };
                let snapshot = cx.background_spawn(async move {
                    if !state.backend.is_trusted() {
                        bail!("Trust this workspace before snapshotting jj");
                    }
                    if !state.fs.is_dir(&root.join(".jj/repo")).await {
                        bail!("jj integration currently supports only colocated single-workspace repositories");
                    }
                    let binary = which::which_in("jj", state.environment.get("PATH"), &root)
                        .context("jj executable not found in the project environment")?;
                    let backend = JjRepository::new(
                        root,
                        binary,
                        state.environment.iter().map(|(key, value)| (key.clone(), value.clone())),
                    );
                    backend.read_session().await?;
                    if !state.backend.is_trusted() {
                        bail!("Trust this workspace before snapshotting jj");
                    }
                    let session = backend.private_read_session().await?;
                    let revision = session.revision("@").await?;
                    let base_revision = revision.parents.first().cloned().unwrap_or_default();
                    let baseline_error = (revision.parents.len() != 1).then(|| {
                        "jj live diffs do not yet support merged-parent changes".to_owned()
                    });
                    Ok::<_, anyhow::Error>(Arc::new(JjSnapshot {
                        backend,
                        session,
                        revision,
                        base_revision,
                        base_files: Arc::new(HashMap::default()),
                        current_files: Arc::new(HashMap::default()),
                        baseline_error,
                        bookmarks: Arc::new(Vec::new()),
                        bookmark_error: None,
                        nearby_changes: Arc::new(Vec::new()),
                        change_error: None,
                        statuses: Arc::new(HashMap::default()),
                    }))
                }).await?;
                if this.update(cx, |this, _| this.repository_state_generation == generation)? {
                    return Ok(snapshot);
                }
            }
        })
    }

    pub fn comparison_blob_contents(
        &mut self,
        session: JjReadSession,
        objects: BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<BTreeMap<String, Arc<[u8]>>>> {
        let mut cached = HashMap::default();
        let mut pending = BTreeMap::new();
        let mut missing = BTreeMap::new();
        for object in objects.values() {
            if let Some(content) = self.comparison_blobs.get(object) {
                cached.insert(object.clone(), content.clone());
            } else if let Some(task) = self.loading_comparison_blobs.get(object) {
                pending.insert(object.clone(), task.clone());
            } else {
                missing.insert(object.clone(), object.clone());
            }
        }
        if !missing.is_empty() {
            let task = cx
                .background_spawn({
                    let missing = missing.clone();
                    async move {
                        session
                            .blob_contents(&missing)
                            .await
                            .map(|contents| {
                                Arc::new(
                                    contents
                                        .into_iter()
                                        .map(|(object, content)| (object, Arc::from(content)))
                                        .collect::<ComparisonBlobs>(),
                                )
                            })
                            .map_err(|error| format!("{error:#}"))
                    }
                })
                .shared();
            for object in missing.keys() {
                self.loading_comparison_blobs
                    .insert(object.clone(), task.clone());
                pending.insert(object.clone(), task.clone());
            }
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, _| {
                    for object in missing.keys() {
                        this.loading_comparison_blobs.remove(object);
                    }
                    if let Ok(contents) = result {
                        for (object, content) in contents.iter() {
                            if content.len() > COMPARISON_BLOB_CACHE_BYTES {
                                continue;
                            }
                            if this.comparison_blob_bytes + content.len()
                                > COMPARISON_BLOB_CACHE_BYTES
                                || this.comparison_blobs.len() >= COMPARISON_BLOB_CACHE_ENTRIES
                            {
                                this.comparison_blobs.clear();
                                this.comparison_blob_bytes = 0;
                            }
                            if !this.comparison_blobs.contains_key(object) {
                                this.comparison_blob_bytes += content.len();
                                this.comparison_blobs
                                    .insert(object.clone(), content.clone());
                            }
                        }
                    }
                })
                .log_err();
            })
            .detach();
        }
        cx.spawn(async move |_, _| {
            for (object, task) in pending {
                let contents = task.await.map_err(anyhow::Error::msg)?;
                let content = contents
                    .get(&object)
                    .context("Git batch omitted requested blob")?;
                cached.insert(object, content.clone());
            }
            objects
                .into_iter()
                .map(|(path, object)| {
                    let content = cached.get(&object).context("missing comparison blob")?;
                    Ok((path, content.clone()))
                })
                .collect()
        })
    }

    pub fn comparison_files(
        &mut self,
        session: JjReadSession,
        base: String,
        head: String,
        mut scopes: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<Vec<JjFileChange>>>> {
        scopes.sort();
        scopes.dedup();
        let key = (base.clone(), head.clone(), scopes.clone());
        Self::comparison_read(
            &mut self.comparison_files,
            key,
            async move { session.comparison_files_scoped(&base, &head, &scopes).await },
            cx,
            |this| &mut this.comparison_files,
        )
    }

    pub fn comparison_tree_entries(
        &mut self,
        session: JjReadSession,
        revision: String,
        mut paths: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<Vec<JjTreeEntry>>>> {
        paths.sort();
        paths.dedup();
        let key = (revision.clone(), paths.clone());
        Self::comparison_read(
            &mut self.comparison_trees,
            key,
            async move { session.tree_entries_exact(&revision, &paths).await },
            cx,
            |this| &mut this.comparison_trees,
        )
    }

    fn comparison_read<K, T>(
        cache: &mut ComparisonReadCache<K, T>,
        key: K,
        read: impl Future<Output = Result<Vec<T>>> + Send + 'static,
        cx: &mut Context<Self>,
        select_cache: fn(&mut Self) -> &mut ComparisonReadCache<K, T>,
    ) -> Task<Result<Arc<Vec<T>>>>
    where
        K: Clone + Eq + Hash + Send + 'static,
        T: Send + Sync + 'static,
    {
        if let Some(contents) = cache.contents.get(&key) {
            return Task::ready(Ok(contents.clone()));
        }
        let task = if let Some(task) = cache.pending.get(&key) {
            task.clone()
        } else {
            let task = cx
                .background_spawn(async move {
                    read.await
                        .map(Arc::new)
                        .map_err(|error| format!("{error:#}"))
                })
                .shared();
            cache.pending.insert(key.clone(), task.clone());
            cx.spawn({
                let task = task.clone();
                async move |this, cx| {
                    let result = task.await;
                    this.update(cx, |this, _| {
                        let cache = select_cache(this);
                        cache.pending.remove(&key);
                        if let Ok(contents) = result {
                            if contents.len() > COMPARISON_LIST_CACHE_ENTRIES {
                                return;
                            }
                            if cache.contents.len() >= COMPARISON_LIST_CACHE_REQUESTS
                                || cache.entries + contents.len() > COMPARISON_LIST_CACHE_ENTRIES
                            {
                                cache.contents.clear();
                                cache.entries = 0;
                            }
                            cache.entries += contents.len();
                            cache.contents.insert(key, contents);
                        }
                    })
                    .log_err();
                }
            })
            .detach();
            task
        };
        cx.spawn(async move |_, _| task.await.map_err(anyhow::Error::msg))
    }

    pub fn new_change(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.mutate(JjMutation::New, cx)
    }

    pub fn edit_change(&mut self, commit_id: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.mutate(JjMutation::Edit(commit_id), cx)
    }

    fn mutate(&mut self, mutation: JjMutation, cx: &mut Context<Self>) -> Task<Result<()>> {
        let mutation_lock = self.mutation_lock.clone();
        cx.spawn(async move |this, cx| {
            let _guard = mutation_lock.lock().await;
            let result = async {
                loop {
                    let (state, generation, root) = this.update(cx, |this, _| {
                        (
                            this.repository_state.clone(),
                            this.repository_state_generation,
                            this.root.clone(),
                        )
                    })?;
                    let state = state.await;
                    if !this.update(cx, |this, _| {
                        this.repository_state_generation == generation
                    })? {
                        continue;
                    }
                    let RepositoryState::Local(state) = state.map_err(anyhow::Error::msg)?
                    else {
                        bail!("jj mutations require a local workspace");
                    };
                    let (backend, trust_backend) = cx.background_spawn(async move {
                        if !state.backend.is_trusted() {
                            bail!("Trust this workspace before changing jj");
                        }
                        if !state.fs.is_dir(&root.join(".jj/repo")).await {
                            bail!("jj integration currently supports only colocated single-workspace repositories");
                        }
                        let binary = which::which_in("jj", state.environment.get("PATH"), &root)
                            .context("jj executable not found in the project environment")?;
                        let backend = JjRepository::new(
                            root,
                            binary,
                            state.environment.iter().map(|(key, value)| (key.clone(), value.clone())),
                        );
                        backend.read_session().await?;
                        Ok::<_, anyhow::Error>((backend, state.backend))
                    }).await?;
                    if !this.update(cx, |this, _| {
                        this.repository_state_generation == generation
                    })? {
                        continue;
                    }
                    break cx.background_spawn(async move {
                        if !trust_backend.is_trusted() {
                            bail!("Trust this workspace before changing jj");
                        }
                        match mutation {
                            JjMutation::Snapshot => backend.snapshot_working_copy().await,
                            JjMutation::New => backend.new_change().await,
                            JjMutation::Edit(commit_id) => backend.edit_change(&commit_id).await,
                        }
                    }).await;
                }
            }.await;
            this.update(cx, |this, cx| {
                this.refresh(cx);
                this.notify_statuses(cx);
                cx.notify();
            }).log_err();
            result
        })
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
                        let task = this
                            .update(cx, |this, cx| {
                                this.snapshot_pending = false;
                                this.mutate(JjMutation::Snapshot, cx)
                            })
                            .map_err(|error| format!("{error:#}"))?;
                        let result = task.await;
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
        let snapshot = self.snapshot.as_ref()?;
        let entry = self
            .diffs
            .values()
            .find(|entry| entry.generation == self.generation && entry.path == path);
        let diff = entry.and_then(|entry| entry.diff.upgrade());
        let Some(diff) = diff else {
            return snapshot.statuses.get(path).copied();
        };
        let diff = diff.read(cx);
        let (added, deleted) = diff.changed_row_counts();
        let ignored = entry
            .and_then(|entry| entry.buffer.upgrade())
            .is_some_and(|buffer| buffer_is_ignored(buffer.read(cx), cx));
        live_file_status(
            diff.base_text_exists(),
            snapshot.current_files.contains_key(path),
            ignored,
            added,
            deleted,
        )
    }

    pub fn display_statuses(&self, cx: &App) -> Vec<StatusEntry> {
        let mut paths = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.statuses.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        paths.extend(
            self.diffs
                .values()
                .filter(|entry| entry.generation == self.generation)
                .map(|entry| entry.path.clone()),
        );
        paths.sort();
        paths.dedup();
        let mut entries = paths
            .into_iter()
            .filter_map(|path| {
                Some(StatusEntry {
                    repo_path: RepoPath::new(&path).log_err()?,
                    status: self.status_for_path(&path, cx)?,
                    diff_stat: None,
                    staged_diff_stat: None,
                    unstaged_diff_stat: None,
                })
            })
            .collect::<Vec<_>>();
        sort_status_entries(&mut entries);
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

fn sort_status_entries(entries: &mut [StatusEntry]) {
    // SumTree cursors seek by path components, not the lexicographic order of slash-separated strings.
    entries.sort_by(|left, right| left.repo_path.cmp(&right.repo_path));
}

fn buffer_is_ignored(buffer: &Buffer, cx: &App) -> bool {
    worktree::File::from_dyn(buffer.file())
        .and_then(|file| file.worktree.read(cx).entry_for_path(&file.path))
        .is_some_and(|entry| entry.is_ignored)
}

fn live_file_status(
    base_exists: bool,
    current_exists: bool,
    ignored: bool,
    added: u32,
    deleted: u32,
) -> Option<FileStatus> {
    if !base_exists && !current_exists {
        return (!ignored).then_some(FileStatus::Untracked);
    }
    if base_exists && added == 0 && deleted == 0 {
        return None;
    }
    Some(FileStatus::worktree(if base_exists {
        StatusCode::Modified
    } else {
        StatusCode::Added
    }))
}

fn change_statuses(changes: Vec<JjFileChange>) -> Result<HashMap<String, FileStatus>> {
    changes
        .into_iter()
        .map(|change| {
            let status = if change.target.conflict {
                FileStatus::Unmerged(UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                })
            } else {
                FileStatus::worktree(match change.status.as_str() {
                    "added" => StatusCode::Added,
                    "removed" => StatusCode::Deleted,
                    "modified" if change.source.file_type != change.target.file_type => {
                        StatusCode::TypeChanged
                    }
                    "modified" => StatusCode::Modified,
                    "renamed" => StatusCode::Renamed,
                    "copied" => StatusCode::Copied,
                    status => bail!("Unsupported jj file status: {status}"),
                })
            };
            Ok((change.path, status))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_entries_use_path_component_order() -> Result<()> {
        let mut entries = [
            "crates/inbx-cli/src/main.rs",
            "crates/inbx/Cargo.toml",
            "crates/inbx/src/anchors.rs",
        ]
        .into_iter()
        .map(|path| {
            Ok(StatusEntry {
                repo_path: RepoPath::new(path)?,
                status: FileStatus::worktree(StatusCode::Added),
                diff_stat: None,
                staged_diff_stat: None,
                unstaged_diff_stat: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
        sort_status_entries(&mut entries);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.repo_path.as_unix_str())
                .collect::<Vec<_>>(),
            [
                "crates/inbx/Cargo.toml",
                "crates/inbx/src/anchors.rs",
                "crates/inbx-cli/src/main.rs",
            ]
        );
        Ok(())
    }

    #[test]
    fn live_status_respects_ignores_only_for_untracked_files() {
        assert_eq!(live_file_status(false, false, true, 1, 0), None);
        assert_eq!(
            live_file_status(false, false, false, 1, 0),
            Some(FileStatus::Untracked)
        );
        assert_eq!(
            live_file_status(true, true, true, 1, 0),
            Some(FileStatus::worktree(StatusCode::Modified))
        );
        assert_eq!(
            live_file_status(false, true, true, 1, 0),
            Some(FileStatus::worktree(StatusCode::Added))
        );
    }

    #[gpui::test]
    async fn ignored_buffer_uses_worktree_ignore_rules(cx: &mut gpui::TestAppContext) {
        use crate::Project;
        use fs::FakeFs;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                ".gitignore": "target/\n",
                "target": { "cache.json": "{}" },
                "new.rs": "fn main() {}",
            }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let ignored = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/project/target/cache.json", cx)
            })
            .await
            .expect("ignored file should open");
        let untracked = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/project/new.rs", cx)
            })
            .await
            .expect("untracked file should open");
        cx.update(|cx| {
            assert!(buffer_is_ignored(ignored.read(cx), cx));
            assert!(!buffer_is_ignored(untracked.read(cx), cx));
        });
    }

    fn change(path: &str, status: &str) -> JjFileChange {
        let file = JjFile {
            path: path.into(),
            file_type: "file".into(),
            conflict: false,
        };
        JjFileChange {
            path: path.into(),
            status: status.into(),
            source: file.clone(),
            target: file,
        }
    }

    #[test]
    fn change_statuses_include_unopened_files() -> Result<()> {
        let statuses = change_statuses(vec![
            change("added.rs", "added"),
            change("modified.rs", "modified"),
            change("deleted.rs", "removed"),
        ])?;
        assert_eq!(statuses.len(), 3);
        assert_eq!(
            statuses.get("added.rs"),
            Some(&FileStatus::worktree(StatusCode::Added))
        );
        assert_eq!(
            statuses.get("modified.rs"),
            Some(&FileStatus::worktree(StatusCode::Modified))
        );
        assert_eq!(
            statuses.get("deleted.rs"),
            Some(&FileStatus::worktree(StatusCode::Deleted))
        );
        assert!(change_statuses(Vec::new())?.is_empty());
        Ok(())
    }

    #[test]
    fn change_statuses_preserve_conflicts_and_type_changes() -> Result<()> {
        let mut conflicted = change("conflict.rs", "modified");
        conflicted.target.conflict = true;
        let mut type_changed = change("link", "modified");
        type_changed.target.file_type = "symlink".into();
        let statuses = change_statuses(vec![conflicted, type_changed])?;
        assert!(matches!(
            statuses.get("conflict.rs"),
            Some(FileStatus::Unmerged(_))
        ));
        assert_eq!(
            statuses.get("link"),
            Some(&FileStatus::worktree(StatusCode::TypeChanged))
        );
        assert!(change_statuses(vec![change("unknown", "unknown")]).is_err());
        Ok(())
    }
}
