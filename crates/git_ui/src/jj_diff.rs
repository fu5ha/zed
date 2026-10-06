use crate::jj_bookmark_picker::{self, JjBookmarkSelection};
use crate::jj_diff_buffer_list::{JjDiffBufferList, LoadedDiff, ResolvedComparison};
use anyhow::{Context as _, Result, bail};
use collections::{HashMap, HashSet};
use editor::{Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, SplittableEditor};
use futures::{StreamExt as _, stream};
use futures_lite::future::yield_now;
use gpui::{
    App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity,
};
use language::{Buffer, BufferEvent, Capability, OffsetRangeExt as _};
use multi_buffer::{MultiBuffer, PathKey};
use project::{Project, ProjectItem as _, git_store::Repository, jj_store::JjStore};
use settings::Settings as _;
use std::{any::TypeId, sync::Arc, time::Duration};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemBufferKind, ItemEvent, SaveOptions},
    notifications::NotifyTaskExt,
    searchable::SearchableItemHandle,
};

#[derive(Clone)]
pub enum Comparison {
    Current,
    Change(String),
    Bookmark(JjBookmarkSelection),
    WorkingCopyChange(String),
    WorkingCopyBookmark(JjBookmarkSelection),
}

impl Comparison {
    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Current, Self::Current) => true,
            (Self::Change(left), Self::Change(right))
            | (Self::WorkingCopyChange(left), Self::WorkingCopyChange(right)) => left == right,
            (Self::Bookmark(left), Self::Bookmark(right))
            | (Self::WorkingCopyBookmark(left), Self::WorkingCopyBookmark(right)) => {
                // Unambiguous bookmark comparisons follow the bookmark as it moves;
                // only an explicitly selected conflicted target is part of their identity.
                let selected_target = |selection: &JjBookmarkSelection| {
                    (selection.bookmark.conflict || selection.bookmark.targets.len() != 1)
                        .then(|| selection.commit_id.clone())
                };
                left.bookmark.name == right.bookmark.name
                    && left.bookmark.remote == right.bookmark.remote
                    && selected_target(left) == selected_target(right)
            }
            _ => false,
        }
    }
}

pub struct JjDiffView {
    comparison: Comparison,
    repository: Entity<Repository>,
    project: Entity<Project>,
    store: Entity<JjStore>,
    editor: Entity<SplittableEditor>,
    paths: HashSet<PathKey>,
    displayed_buffers: HashMap<PathKey, Entity<Buffer>>,
    retained_paths: HashSet<PathKey>,
    title: SharedString,
    error: Option<String>,
    loading: bool,
    task: Task<()>,
    subscriptions: Vec<Subscription>,
    buffer_subscriptions: HashMap<language::BufferId, Subscription>,
    window_context: gpui::AsyncWindowContext,
    live_diffs: HashMap<language::BufferId, (Entity<buffer_diff::BufferDiff>, PathKey)>,
    recalculations: HashMap<language::BufferId, Task<()>>,
    metadata_pending: bool,
    invalidated_paths: HashSet<String>,
    path_generations: HashMap<String, u64>,
    generation: u64,
    resolved: Option<ResolvedComparison>,
    loader: JjDiffBufferList,
    requested_path: Option<String>,
    published_operation: Option<String>,
    observed_versions: HashMap<language::BufferId, clock::Global>,
}

impl JjDiffView {
    pub fn open(
        workspace: &mut Workspace,
        comparison: Comparison,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            workspace.show_error("No active repository", cx);
            return;
        };
        let Some(store) = repository.read(cx).jj.clone() else {
            workspace.show_error("The active repository is not a local jj workspace", cx);
            return;
        };
        let existing = workspace.items_of_type::<Self>(cx).find(|view| {
            let view = view.read(cx);
            view.repository == repository && view.comparison.matches(&comparison)
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let requested_path = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            .and_then(|editor| editor.read(cx).active_buffer(cx))
            .and_then(|buffer| {
                let buffer = buffer.read(cx);
                let absolute = buffer.file()?.as_local()?.abs_path(cx);
                absolute
                    .strip_prefix(&repository.read(cx).work_directory_abs_path)
                    .ok()?
                    .to_str()
                    .map(|path| path.replace('\\', "/"))
            });
        let workspace_entity = cx.entity();
        let view = cx.new(|cx| {
            let multibuffer = cx.new(|cx| {
                let mut buffer = MultiBuffer::new(if matches!(comparison, Comparison::Change(_)) {
                    Capability::ReadOnly
                } else {
                    Capability::ReadWrite
                });
                buffer.set_all_diff_hunks_expanded(cx);
                buffer
            });
            let editor = cx.new(|cx| {
                SplittableEditor::new(
                    EditorSettings::get_global(cx).diff_view_style,
                    multibuffer,
                    project.clone(),
                    workspace_entity.clone(),
                    window,
                    cx,
                )
            });
            let loader =
                JjDiffBufferList::new(project.clone(), repository.clone(), store.clone(), cx);
            let mut view = Self {
                comparison,
                repository,
                project,
                store: store.clone(),
                editor,
                paths: HashSet::default(),
                displayed_buffers: HashMap::default(),
                retained_paths: HashSet::default(),
                title: "jj diff".into(),
                error: None,
                loading: false,
                task: Task::ready(()),
                subscriptions: Vec::new(),
                buffer_subscriptions: HashMap::default(),
                window_context: window.to_async(cx),
                live_diffs: HashMap::default(),
                recalculations: HashMap::default(),
                metadata_pending: false,
                invalidated_paths: HashSet::default(),
                path_generations: HashMap::default(),
                generation: 0,
                resolved: None,
                loader,
                requested_path,
                published_operation: store
                    .read(cx)
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.session.operation_id().to_owned()),
                observed_versions: HashMap::default(),
            };
            view.editor.update(cx, |editor, cx| {
                editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
            });
            view.subscriptions.push(
                cx.subscribe(&view.editor, |_: &mut Self, _, event: &EditorEvent, cx| {
                    cx.emit(event.clone())
                }),
            );
            if !matches!(view.comparison, Comparison::Change(_)) {
                let buffers = view.project.read(cx).buffer_store().clone();
                view.subscriptions
                    .push(cx.subscribe(&buffers, |view, _, event, cx| match event {
                        project::buffer_store::BufferStoreEvent::BufferAdded(buffer) => {
                            view.subscribe_buffer(buffer.clone(), cx);
                            if buffer.read(cx).is_dirty() {
                                view.observed_versions.remove(&buffer.read(cx).remote_id());
                                view.buffer_changed(buffer.clone(), cx);
                            }
                        }
                        project::buffer_store::BufferStoreEvent::BufferChangedFilePath {
                            ..
                        } => {
                            view.request_reload(cx);
                        }
                        _ => {}
                    }));
                let existing_buffers = buffers.read(cx).buffers().collect::<Vec<_>>();
                for buffer in existing_buffers {
                    view.subscribe_buffer(buffer, cx);
                }
                view.subscriptions
                    .push(cx.observe(&view.store, |view, _, cx| {
                        // Published sessions, not snapshot command completion, trigger reconciliation.
                        let operation = view
                            .store
                            .read(cx)
                            .snapshot
                            .as_ref()
                            .map(|snapshot| snapshot.session.operation_id().to_owned());
                        if operation != view.published_operation {
                            view.published_operation = operation;
                            view.request_reload(cx);
                        }
                    }));
                let git_store = view.project.read(cx).git_store().clone();
                view.subscriptions
                    .push(cx.subscribe(&git_store, |view, _, event, cx| {
                        if matches!(
                            event,
                            project::git_store::GitStoreEvent::RepositoryAdded
                                | project::git_store::GitStoreEvent::RepositoryRemoved(_)
                        ) {
                            view.loader = JjDiffBufferList::new(
                                view.project.clone(),
                                view.repository.clone(),
                                view.store.clone(),
                                cx,
                            );
                            view.request_reload(cx);
                        }
                    }));
                let project = view.project.clone();
                view.subscriptions
                    .push(cx.subscribe(&project, |view, _, event, cx| match event {
                        project::Event::WorktreeAdded(_)
                        | project::Event::WorktreeRemoved(_)
                        | project::Event::WorktreePathsChanged { .. } => {
                            view.loader = JjDiffBufferList::new(
                                view.project.clone(),
                                view.repository.clone(),
                                view.store.clone(),
                                cx,
                            );
                            view.request_reload(cx);
                        }
                        project::Event::WorktreeUpdatedEntries(worktree_id, changes) => {
                            if let Some(worktree) =
                                view.project.read(cx).worktree_for_id(*worktree_id, cx)
                            {
                                let root = &view.repository.read(cx).work_directory_abs_path;
                                let tree = worktree.read(cx).snapshot();
                                let paths = changes
                                    .iter()
                                    .filter_map(|(path, _, change)| {
                                        // Initial scan membership is not a content change; the
                                        // private snapshot discovers preexisting disk changes.
                                        if *change == project::PathChange::Loaded {
                                            return None;
                                        }
                                        let absolute = tree.abs_path().join(path.as_std_path());
                                        absolute
                                            .strip_prefix(root)
                                            .ok()?
                                            .to_str()
                                            .map(|path| path.replace('\\', "/"))
                                    })
                                    .collect::<Vec<_>>();
                                for path in paths {
                                    if view.loader.filter_path(&path) {
                                        view.invalidate_path(path, cx);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }));
            }
            view.start_update(true, cx);
            view
        });
        workspace.active_pane().update(cx, |pane, cx| {
            pane.add_item(Box::new(view), true, true, None, window, cx);
        });
    }

    fn subscribe_buffer(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let buffer_id = buffer.read(cx).remote_id();
        if self.buffer_subscriptions.contains_key(&buffer_id) {
            return;
        }
        self.observed_versions
            .insert(buffer_id, buffer.read(cx).text_snapshot().version().clone());
        let subscription = cx.subscribe(&buffer, |view, buffer, event, cx| {
            if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                view.buffer_changed(buffer, cx);
            }
        });
        self.buffer_subscriptions.insert(buffer_id, subscription);
    }

    fn invalidate_path(&mut self, path: String, cx: &mut Context<Self>) {
        *self.path_generations.entry(path.clone()).or_default() += 1;
        self.invalidated_paths.insert(path);
        if !self.loading {
            self.start_update(false, cx);
        }
    }

    fn request_reload(&mut self, cx: &mut Context<Self>) {
        self.metadata_pending = true;
        // Invalidate ongoing loads before a newer session can change their baselines.
        self.generation += 1;
        if !self.loading {
            self.start_update(false, cx);
        }
    }

    fn start_update(&mut self, initial: bool, cx: &mut Context<Self>) {
        self.loading = true;
        let refresh_metadata = initial || std::mem::take(&mut self.metadata_pending);
        let published = self.store.read(cx).snapshot.clone().map(Arc::new);
        let snapshot_task = if initial && !matches!(self.comparison, Comparison::Change(_)) {
            Some(
                self.store
                    .update(cx, |store, cx| store.private_snapshot(cx)),
            )
        } else {
            None
        };
        let comparison = self.comparison.clone();
        let loader = self.loader.clone();
        let store = self.store.clone();
        let previous = self.resolved.clone();
        let generation = self.generation;
        let requested_path = self.requested_path.clone();
        let this = cx.weak_entity();
        self.task = self.window_context.spawn(async move |cx| {
            // Only event bursts are coalesced; tab creation and first discovery never wait.
            if !initial {
                cx.background_executor()
                    .timer(Duration::from_millis(25))
                    .await;
            }
            let result: Result<()> = async {
                let resolved = if refresh_metadata || previous.is_none() {
                    let snapshot = match snapshot_task {
                        Some(task) => task.await?,
                        None => published.clone().context("jj state is loading")?,
                    };
                    match loader
                        .discover(&comparison, snapshot, published.clone(), cx)
                        .await
                    {
                        Ok(resolved) => resolved,
                        Err(error)
                            if initial
                                && !matches!(comparison, Comparison::Change(_))
                                && format!("{error:#}").to_lowercase().contains("operation") =>
                        {
                            // Private operations are collectible. Retry discovery once, without
                            // publishing the replacement or depending on its lifetime afterward.
                            let snapshot = store
                                .update(cx, |store, cx| store.private_snapshot(cx))
                                .await?;
                            loader
                                .discover(&comparison, snapshot, published, cx)
                                .await?
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    previous.context("missing jj comparison")?
                };
                let (mut paths, versions) = this.update(cx, |this, cx| {
                    let mut paths = std::mem::take(&mut this.invalidated_paths);
                    if refresh_metadata {
                        paths.extend(resolved.paths.iter().cloned());
                        paths.extend(
                            this.paths
                                .iter()
                                .map(|path| path.path.as_unix_str().to_owned()),
                        );
                        if resolved.live {
                            paths.extend(loader.dirty_paths(cx));
                        }
                    }
                    let versions = paths
                        .iter()
                        .map(|path| {
                            (
                                path.clone(),
                                this.path_generations.get(path).copied().unwrap_or_default(),
                            )
                        })
                        .collect::<HashMap<_, _>>();
                    (paths.into_iter().collect::<Vec<_>>(), versions)
                })?;
                paths.sort_by(|left, right| {
                    (Some(left) != requested_path.as_ref())
                        .cmp(&(Some(right) != requested_path.as_ref()))
                        .then_with(|| left.cmp(right))
                });
                let candidates = match loader.candidates(&resolved, paths.clone(), cx).await {
                    Ok(candidates) => candidates,
                    Err(error) => {
                        this.update(cx, |this, _| {
                            this.invalidated_paths.extend(paths);
                        })?;
                        return Err(error);
                    }
                };
                this.update(cx, |this, _| {
                    if this.generation == generation {
                        this.resolved = Some(resolved.clone());
                    }
                })?;
                let load_context = cx.clone();
                let mut loads = stream::iter(candidates.into_iter().map(|candidate| {
                    let loader = loader.clone();
                    let resolved = resolved.clone();
                    let mut cx = load_context.clone();
                    async move {
                        let path = candidate.path.clone();
                        (path, loader.load(&resolved, candidate, &mut cx).await)
                    }
                }))
                .buffer_unordered(16);
                let mut completed = HashSet::default();
                let mut errors = Vec::new();
                while let Some((path, loaded)) = loads.next().await {
                    yield_now().await;
                    completed.insert(path.clone());
                    match loaded {
                        Ok(loaded) => {
                            this.update_in(cx, |this, window, cx| {
                                let expected = versions.get(&path).copied().unwrap_or_default();
                                if this.generation != generation
                                    || this
                                        .path_generations
                                        .get(&path)
                                        .copied()
                                        .unwrap_or_default()
                                        != expected
                                {
                                    this.invalidated_paths.insert(path);
                                    return;
                                }
                                if loaded.as_ref().is_some_and(|loaded| {
                                    loaded.buffer.read(cx).text_snapshot().version()
                                        != &loaded.version
                                }) {
                                    this.invalidated_paths.insert(path);
                                    return;
                                }
                                this.present(&path, loaded, resolved.live, window, cx);
                            })?;
                        }
                        Err(error) => errors.push(format!("{path}: {error:#}")),
                    }
                }
                this.update(cx, |this, cx| {
                    if this.generation != generation {
                        this.invalidated_paths.extend(paths);
                        return;
                    }
                    for path in paths.into_iter().filter(|path| !completed.contains(path)) {
                        if this
                            .path_generations
                            .get(&path)
                            .copied()
                            .unwrap_or_default()
                            == versions.get(&path).copied().unwrap_or_default()
                        {
                            this.remove_path(&path, cx);
                        }
                    }
                    this.title = resolved.title.clone().into();
                    this.error = (!errors.is_empty()).then(|| errors.join("\n"));
                    cx.emit(EditorEvent::TitleChanged);
                })?;
                Ok(())
            }
            .await;
            let succeeded = result.is_ok();
            this.update(cx, |this, cx| {
                this.loading = false;
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}"));
                }
                cx.notify();
                if succeeded && (this.metadata_pending || !this.invalidated_paths.is_empty()) {
                    this.start_update(false, cx);
                }
            })
            .log_err();
        });
        cx.notify();
    }

    fn remove_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(key) = self
            .paths
            .iter()
            .find(|key| key.path.as_unix_str() == path)
            .cloned()
        else {
            return;
        };
        if self
            .displayed_buffers
            .get(&key)
            .is_some_and(|buffer| buffer.read(cx).is_dirty())
        {
            self.retained_paths.insert(key);
            return;
        }
        self.editor.update(cx, |editor, cx| {
            editor.remove_excerpts_for_path(key.clone(), cx)
        });
        self.paths.remove(&key);
        self.retained_paths.remove(&key);
        self.displayed_buffers.remove(&key);
        self.live_diffs.retain(|_, (_, path)| path != &key);
    }

    fn present(
        &mut self,
        path: &str,
        loaded: Option<LoadedDiff>,
        live: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(loaded) = loaded else {
            self.remove_path(path, cx);
            return;
        };
        let LoadedDiff {
            key,
            buffer,
            diff,
            ranges,
            ..
        } = loaded;
        if ranges.is_empty() {
            self.remove_path(path, cx);
            return;
        }
        if self
            .displayed_buffers
            .get(&key)
            .is_some_and(|previous| previous != &buffer && previous.read(cx).is_dirty())
        {
            self.retained_paths.insert(key);
            return;
        }
        if let Some(previous_path) =
            self.displayed_buffers
                .iter()
                .find_map(|(previous_path, previous)| {
                    (previous_path != &key && previous == &buffer && previous.read(cx).is_dirty())
                        .then(|| previous_path.clone())
                })
        {
            self.retained_paths.insert(previous_path);
            return;
        }
        self.paths.insert(key.clone());
        self.retained_paths.remove(&key);
        let buffer_id = buffer.read(cx).remote_id();
        self.recalculations.remove(&buffer_id);
        self.live_diffs.retain(|previous_id, (_, previous_path)| {
            previous_path != &key || *previous_id == buffer_id
        });
        self.displayed_buffers.insert(key.clone(), buffer.clone());
        if live {
            self.live_diffs
                .insert(buffer_id, (diff.clone(), key.clone()));
            self.subscribe_buffer(buffer.clone(), cx);
        }
        self.editor.update(cx, |editor, cx| {
            let rhs_editor = editor.rhs_editor().clone();
            let scroll_anchor = rhs_editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                let mut anchor = editor.scroll_manager.native_anchor(&snapshot, cx);
                if !snapshot.buffer_snapshot().is_empty()
                    && anchor.anchor == multi_buffer::Anchor::Min
                {
                    anchor.anchor = snapshot
                        .buffer_snapshot()
                        .anchor_before(language::Point::zero());
                }
                anchor
            });
            editor.update_excerpts_for_path(key, buffer, ranges, 3, diff, cx);
            if editor.diff_view_style() == settings::DiffViewStyle::Split {
                editor.split(window, cx);
            }
            rhs_editor.update(cx, |editor, cx| {
                if scroll_anchor
                    .anchor
                    .is_valid(&editor.buffer().read(cx).snapshot(cx))
                {
                    editor.set_scroll_anchor(scroll_anchor, window, cx);
                }
            });
        });
        cx.notify();
    }

    fn buffer_changed(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let value = buffer.read(cx);
        let buffer_id = value.remote_id();
        let version = value.text_snapshot().version().clone();
        if self
            .observed_versions
            .insert(buffer_id, version.clone())
            .as_ref()
            == Some(&version)
        {
            return;
        }
        let Some(file) = value.file().and_then(|file| file.as_local()) else {
            return;
        };
        let absolute = file.abs_path(cx);
        let root = &self.repository.read(cx).work_directory_abs_path;
        let Some(path) = absolute
            .strip_prefix(root)
            .ok()
            .and_then(|path| path.to_str())
            .map(|path| path.replace('\\', "/"))
        else {
            return;
        };
        if !self.loader.filter_path(&path) {
            return;
        }
        *self.path_generations.entry(path.clone()).or_default() += 1;
        let Some((diff, key)) = self.live_diffs.get(&buffer_id).cloned() else {
            self.invalidate_path(path, cx);
            return;
        };
        if self.loading {
            self.invalidated_paths.insert(path.clone());
        }
        let generation = self.generation;
        let calculation = diff.read(cx).update_diff(
            value.text_snapshot(),
            &diff.read(cx).base_text(cx),
            diff.read(cx).base_text_string(cx).map(Arc::from),
            cx,
        );
        let task = cx.spawn(async move |this, cx| {
            let update = calculation.await;
            this.update(cx, |this, cx| {
                if this.generation != generation
                    || buffer.read(cx).text_snapshot().version() != &version
                {
                    return;
                }
                if this
                    .live_diffs
                    .get(&buffer_id)
                    .is_none_or(|(current, _)| current != &diff)
                {
                    return;
                }
                diff.update(cx, |diff, cx| diff.set_snapshot(update, cx));
                let snapshot = buffer.read(cx).snapshot();
                let mut ranges = diff
                    .read(cx)
                    .snapshot(cx)
                    .hunks(&snapshot)
                    .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                    .collect::<Vec<_>>();
                if ranges.is_empty() && buffer.read(cx).is_dirty() {
                    ranges.push(language::Point::zero()..snapshot.max_point());
                }
                this.editor.update(cx, |editor, cx| {
                    editor.update_excerpts_for_path(key, buffer, ranges, 3, diff, cx);
                });
            })
            .log_err();
        });
        self.recalculations.insert(buffer_id, task);
    }
}

pub(crate) fn selected_bookmark_target(
    snapshot: &project::jj_store::JjSnapshot,
    selection: &JjBookmarkSelection,
) -> Result<(String, String)> {
    let bookmark = snapshot
        .bookmarks
        .iter()
        .find(|bookmark| {
            bookmark.name == selection.bookmark.name && bookmark.remote == selection.bookmark.remote
        })
        .context("Selected jj bookmark no longer exists")?;
    let target = if bookmark.targets.len() == 1 && !bookmark.conflict {
        bookmark
            .targets
            .first()
            .context("jj bookmark has no target")?
    } else if bookmark.targets.contains(&selection.commit_id) {
        &selection.commit_id
    } else {
        bail!("jj bookmark is conflicted or changed ambiguously; select its target again");
    };
    let name = match &bookmark.remote {
        Some(remote) => format!("{}@{remote}", bookmark.name),
        None => bookmark.name.clone(),
    };
    Ok((target.clone(), name))
}

pub fn open_bookmark_picker(
    workspace: &mut Workspace,
    comparison: fn(JjBookmarkSelection) -> Comparison,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
        workspace.show_error("No active repository", cx);
        return;
    };
    let Some(store) = repository.read(cx).jj.clone() else {
        workspace.show_error("The active repository is not a local jj workspace", cx);
        return;
    };
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        jj_bookmark_picker::selection_modal(
            store,
            Box::new(move |selection, window, cx| {
                workspace_handle
                    .update(cx, |workspace, cx| {
                        if require_original_repository(workspace, &repository, cx) {
                            JjDiffView::open(workspace, comparison(selection), window, cx);
                        }
                    })
                    .log_err();
            }),
            window,
            cx,
        )
    });
}

pub fn open_change_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_change_picker_with_mode(workspace, false, window, cx);
}

pub fn open_working_copy_change_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_change_picker_with_mode(workspace, true, window, cx);
}

fn open_change_picker_with_mode(
    workspace: &mut Workspace,
    working_copy: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
        workspace.show_error("No active repository", cx);
        return;
    };
    let Some(store) = repository.read(cx).jj.clone() else {
        workspace.show_error("The active repository is not a local jj workspace", cx);
        return;
    };
    let Some(snapshot) = store.read(cx).snapshot.clone() else {
        return;
    };
    let workspace = cx.entity();
    let weak = workspace.downgrade();
    window
        .spawn(cx, async move |cx| {
            let revisions = snapshot.session.recent_revisions().await?;
            workspace.update_in(cx, |workspace, window, cx| {
                if !require_original_repository(workspace, &repository, cx) {
                    return;
                }
                let handle = cx.weak_entity();
                workspace.toggle_modal(window, cx, move |window, cx| {
                    picker::Picker::uniform_list(
                        ChangeDelegate {
                            revisions,
                            matches: Vec::new(),
                            selected: 0,
                            workspace: handle,
                            working_copy,
                            repository,
                        },
                        window,
                        cx,
                    )
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_notify_err(weak, window, cx);
}

struct ChangeDelegate {
    revisions: Vec<git::jj::JjRevision>,
    matches: Vec<usize>,
    selected: usize,
    workspace: WeakEntity<Workspace>,
    working_copy: bool,
    repository: Entity<Repository>,
}
impl picker::PickerDelegate for ChangeDelegate {
    type ListItem = ui::ListItem;
    fn name() -> &'static str {
        "jj changes"
    }
    fn match_count(&self) -> usize {
        self.matches.len()
    }
    fn selected_index(&self) -> usize {
        self.selected
    }
    fn set_selected_index(
        &mut self,
        index: usize,
        _: &mut Window,
        _: &mut Context<picker::Picker<Self>>,
    ) {
        self.selected = index;
    }
    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        if self.working_copy {
            "Select a baseline jj change (100 most recent)…".into()
        } else {
            "Select a jj change (100 most recent)…".into()
        }
    }
    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<picker::Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.matches = self
            .revisions
            .iter()
            .enumerate()
            .filter(|(_, revision)| {
                revision.change_id.contains(&query)
                    || revision.commit_id.contains(&query)
                    || revision.description.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        cx.notify();
        Task::ready(())
    }
    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<picker::Picker<Self>>) {
        let Some(revision) = self
            .matches
            .get(self.selected)
            .and_then(|index| self.revisions.get(*index))
        else {
            return;
        };
        let commit = revision.commit_id.clone();
        let comparison = if self.working_copy {
            Comparison::WorkingCopyChange(commit)
        } else {
            Comparison::Change(commit)
        };
        self.workspace
            .update(cx, |workspace, cx| {
                if require_original_repository(workspace, &self.repository, cx) {
                    JjDiffView::open(workspace, comparison, window, cx);
                }
            })
            .log_err();
        cx.emit(gpui::DismissEvent);
    }
    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<picker::Picker<Self>>) {
        cx.emit(gpui::DismissEvent);
    }
    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<picker::Picker<Self>>,
    ) -> Option<ui::ListItem> {
        let revision = self.revisions.get(*self.matches.get(index)?)?;
        Some(
            ui::ListItem::new(index)
                .toggle_state(selected)
                .child(Label::new(format!(
                    "{} — {}",
                    revision.change_id.chars().take(8).collect::<String>(),
                    crate::jj_bookmark_picker::change_description(revision)
                ))),
        )
    }
}

fn require_original_repository(
    workspace: &mut Workspace,
    repository: &Entity<Repository>,
    cx: &mut Context<Workspace>,
) -> bool {
    if workspace.project().read(cx).active_repository(cx).as_ref() == Some(repository) {
        true
    } else {
        workspace.show_error(
            "The active repository changed while selecting a jj target. Open the comparison selector again.",
            cx,
        );
        false
    }
}

impl EventEmitter<EditorEvent> for JjDiffView {}
impl Focusable for JjDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}
impl Render for JjDiffView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(
                h_flex().p_2().gap_2().child(Label::new("jj diff")).child(
                    Label::new("Symlinks, submodules and file-mode-only changes are omitted.")
                        .color(Color::Muted),
                ),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    Label::new(if self.paths.is_empty() {
                        error
                    } else {
                        format!("Comparison unavailable; previous contents retained: {error}")
                    })
                    .color(Color::Error),
                )
            })
            .when(!self.retained_paths.is_empty(), |view| {
                let mut paths = self
                    .retained_paths
                    .iter()
                    .map(|path| path.path.as_unix_str())
                    .collect::<Vec<_>>();
                paths.sort();
                view.child(
                    Label::new(format!(
                        "Unsaved drafts retained outside the refreshed comparison: {}",
                        paths.join(", ")
                    ))
                    .color(Color::Warning),
                )
            })
            .child(self.editor.clone())
    }
}
impl Item for JjDiffView {
    type Event = EditorEvent;
    fn capability(&self, cx: &App) -> Capability {
        self.editor.read(cx).capability(cx)
    }
    fn buffer_kind(&self, cx: &App) -> ItemBufferKind {
        self.editor.read(cx).buffer_kind(cx)
    }
    fn for_each_project_item(
        &self,
        cx: &App,
        callback: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        let editor = self.editor.read(cx).rhs_editor().read(cx);
        for buffer in editor.buffer().read(cx).all_buffers() {
            if buffer
                .read(cx)
                .file()
                .is_some_and(|file| file.as_local().is_some())
            {
                callback(buffer.entity_id(), buffer.read(cx));
            }
        }
    }
    fn active_project_path(&self, cx: &App) -> Option<project::ProjectPath> {
        let buffer = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .active_buffer(cx)?;
        let buffer = buffer.read(cx);
        buffer.file()?.as_local()?;
        buffer.project_path(cx)
    }
    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).is_dirty(cx)
    }
    fn has_conflict(&self, cx: &App) -> bool {
        self.editor.read(cx).has_conflict(cx)
    }
    fn has_deleted_file(&self, cx: &App) -> bool {
        self.editor.read(cx).has_deleted_file(cx)
    }
    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
            && self
                .editor
                .read(cx)
                .rhs_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .all_buffers()
                .iter()
                .any(|buffer| {
                    let buffer = buffer.read(cx);
                    !buffer.read_only()
                        && buffer.file().is_some_and(|file| file.as_local().is_some())
                })
    }
    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }
    fn reload(
        &mut self,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffers = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers()
            .into_iter()
            .filter(|buffer| {
                buffer
                    .read(cx)
                    .file()
                    .is_some_and(|file| file.as_local().is_some())
            })
            .collect();
        let task = project.update(cx, |project, cx| project.reload_buffers(buffers, true, cx));
        cx.spawn(async move |this, cx| {
            task.await?;
            this.update(cx, |this, cx| {
                // An explicit reload also reconciles events missed by the worktree watcher.
                this.generation += 1;
                this.start_update(true, cx);
            })?;
            Ok(())
        })
    }
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone()
    }
    fn to_item_events(event: &EditorEvent, callback: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, callback);
    }
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}
