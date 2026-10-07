use crate::{
    Addon, Anchor, DisplayRow, Editor, EditorEvent,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    inbox_settings::InboxSettings,
    inbox_store::{InboxStore, Mutation},
    scroll::Autoscroll,
};
use collections::{HashMap, HashSet};
use gpui::{
    App, Context, Entity, EntityId, Focusable, KeyContext, Subscription, Task, WeakEntity, actions,
    prelude::*,
};
use inbx::{
    anchors::{self, LocateOptions, LocateResult, MatchKind},
    models::{Source, ThreadWithMessages},
};
use markdown::{Markdown, MarkdownElement, MarkdownOptions};
use settings::Settings;
use std::{any::Any, ops::Range, path::PathBuf, sync::Arc};
use text::{Bias, Point, ToPoint as _};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::Tooltip;
use ui::prelude::*;
use util::ResultExt;
use workspace::{Toast, notifications::NotificationId};

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod inbox_tests;

actions!(
    inbox,
    [
        /// Create a Pi Inbox discussion at the selected source lines.
        AddThread,
        /// Browse the shared Pi Inbox.
        ShowThreads,
        /// Show or hide resolved Pi Inbox discussions.
        ToggleResolved,
        /// Refresh Pi Inbox discussions from the shared database.
        Refresh,
        /// Post the message in a Pi Inbox composer.
        Post,
        /// Cancel the active Pi Inbox draft or message edit.
        CancelComposer,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(
        |editor: &mut Editor, window: Option<&mut Window>, cx: &mut Context<Editor>| {
            if !editor.mode().is_full() {
                return;
            }
            let Some(window) = window else { return };
            let owner = cx.entity().downgrade();
            let store = InboxStore::global(cx);
            let state = cx.new(|cx| InboxEditor::new(owner.clone(), store, window, cx));
            editor.register_addon(InboxAddon(state));
            editor
                .register_action(move |_: &AddThread, window, cx| {
                    if let Some(editor) = owner.upgrade() {
                        editor.update(cx, |editor, cx| {
                            editor.add_inbox_thread_at_selection(window, cx);
                        });
                    }
                })
                .detach();
        },
    )
    .detach();
    crate::inbox_browser::init(cx);
}

pub(crate) fn can_comment(editor: &Editor, cx: &App) -> bool {
    InboxSettings::get_global(cx).enabled
        && editor.mode().is_full()
        && editor.addon::<InboxAddon>().is_some()
        && editor
            .project()
            .is_some_and(|project| project.read(cx).is_local())
        && editor.active_buffer(cx).is_some_and(|buffer| {
            buffer.read(cx).file().is_some_and(|file| {
                file.disk_state().exists()
                    && file
                        .as_local()
                        .is_some_and(|file| file.abs_path(cx).is_absolute())
            })
        })
}

pub(crate) fn report_error(editor: &WeakEntity<Editor>, message: String, cx: &mut App) {
    log::error!("Pi Inbox: {message}");
    let editor = editor.clone();
    cx.defer(move |cx| {
        if let Some(workspace) = editor
            .upgrade()
            .and_then(|editor| editor.read(cx).workspace())
        {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<AddThread>(),
                        format!("Pi Inbox: {message}"),
                    ),
                    cx,
                );
            });
        }
    });
}

struct InboxAddon(Entity<InboxEditor>);
struct ComposerAddon;

impl Addon for ComposerAddon {
    fn extend_key_context(&self, context: &mut KeyContext, _: &App) {
        context.add("PiInboxComposer");
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

impl Addon for InboxAddon {
    fn to_any(&self) -> &dyn Any {
        self
    }
}

impl Editor {
    fn add_inbox_thread_at_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.display_snapshot(cx);
        let display_selection = self.selections.newest_display(&snapshot);
        let last_row = display_selection.end.row().0.saturating_sub(u32::from(
            display_selection.end.column() == 0
                && display_selection.end.row() > display_selection.start.row(),
        ));
        if snapshot
            .row_infos(display_selection.start.row())
            .take((last_row - display_selection.start.row().0 + 1) as usize)
            .any(|row| row.diff_status.is_some_and(|status| status.is_deleted()))
        {
            report_error(
                &cx.entity().downgrade(),
                "Deleted source lines cannot receive a thread.".into(),
                cx,
            );
            return;
        }
        let selection = self.selections.newest::<Point>(&snapshot);
        let mut end = selection.end;
        if end.column == 0 && end.row > selection.start.row {
            end.row -= 1;
            end.column = self
                .buffer
                .read(cx)
                .snapshot(cx)
                .line_len(crate::MultiBufferRow(end.row));
        }
        self.create_inbox_draft(selection.start..end, window, cx);
    }

    pub(crate) fn add_inbox_thread_for_display_range(
        &mut self,
        rows: Range<DisplayRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if rows.start >= rows.end {
            report_error(
                &cx.entity().downgrade(),
                "Select at least one source line.".into(),
                cx,
            );
            return;
        }
        let snapshot = self.display_snapshot(cx);
        if snapshot
            .row_infos(rows.start)
            .take((rows.end.0 - rows.start.0) as usize)
            .any(|row| row.diff_status.is_some_and(|status| status.is_deleted()))
        {
            report_error(
                &cx.entity().downgrade(),
                "Deleted source lines cannot receive a thread.".into(),
                cx,
            );
            return;
        }
        let start = snapshot.display_point_to_point(rows.start.as_display_point(), Bias::Left);
        let end = snapshot.display_point_to_point(
            DisplayRow(rows.end.0.saturating_sub(1)).as_display_point(),
            Bias::Left,
        );
        let buffer = self.buffer.read(cx).snapshot(cx);
        self.create_inbox_draft(
            start..Point::new(end.row, buffer.line_len(crate::MultiBufferRow(end.row))),
            window,
            cx,
        );
    }

    fn create_inbox_draft(
        &mut self,
        range: Range<Point>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = (|| {
            if !can_comment(self, cx) {
                return Err("Open a saved local source file first.".to_string());
            }
            let snapshot = self.buffer.read(cx).snapshot(cx);
            let ranges = snapshot.range_to_buffer_ranges(range);
            if ranges.len() != 1 {
                return Err("Select a contiguous range in one source file.".into());
            }
            let Some((buffer, range, _)) = ranges.first() else {
                return Err("No source lines selected.".into());
            };
            let file = buffer
                .file()
                .and_then(|file| file.as_local())
                .ok_or("Open a saved local source file first.")?;
            let file = file.abs_path(cx);
            let start = range.start.to_point(buffer).row as usize;
            let end = range.end.to_point(buffer).row as usize;
            let content_id =
                anchors::create_content_id(&buffer.text(), start + 1, end - start + 1, 3)?;
            let anchor = snapshot
                .anchor_in_excerpt(
                    buffer.anchor_after(Point::new(end as u32, buffer.line_len(end as u32))),
                )
                .ok_or("Selection is outside the displayed source excerpt.")?;
            let state = self
                .addon::<InboxAddon>()
                .ok_or("Inbox unavailable.")?
                .0
                .clone();
            Ok((state, file, content_id, anchor))
        })();
        match result {
            Ok((state, file, content_id, anchor)) => {
                // The addon inserts editor blocks, so wait until this editor update has ended.
                window.defer(cx, move |window, cx| {
                    state.update(cx, |state, cx| {
                        state.add_draft(file, content_id, anchor, window, cx)
                    });
                });
            }
            Err(error) => report_error(&cx.entity().downgrade(), error, cx),
        }
    }

    pub(crate) fn show_inbox_thread(
        &mut self,
        sqid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(addon) = self.addon::<InboxAddon>() {
            let state = addon.0.clone();
            let sqid = sqid.to_string();
            window.defer(cx, move |window, cx| {
                state.update(cx, |state, cx| {
                    state.pending_open = Some(sqid);
                    state.reconcile(window, cx);
                });
            });
        }
    }
}

struct InboxEditor {
    owner: WeakEntity<Editor>,
    store: Entity<InboxStore>,
    threads: HashMap<String, Entity<InboxThread>>,
    drafts: Vec<Entity<InboxThread>>,
    blocks: Vec<CustomBlockId>,
    block_layout: Vec<(Anchor, EntityId, u32)>,
    locations: HashMap<String, LocateResult>,
    inputs: Vec<(String, String, String)>,
    resolving: Task<()>,
    pending_open: Option<String>,
    last_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl InboxEditor {
    fn new(
        owner: WeakEntity<Editor>,
        store: Entity<InboxStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.reconcile(window, cx);
            }),
            cx.observe_global_in::<settings::SettingsStore>(window, |this, window, cx| {
                this.reconcile(window, cx);
            }),
        ];
        if let Some(editor) = owner.upgrade() {
            subscriptions.push(
                cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
                    if matches!(
                        event,
                        EditorEvent::BufferEdited
                            | EditorEvent::FileHandleChanged
                            | EditorEvent::BuffersRemoved { .. }
                            | EditorEvent::BufferRangesUpdated { .. }
                            | EditorEvent::Saved
                    ) {
                        this.reconcile(window, cx);
                    }
                }),
            );
        }
        cx.defer_in(window, |this, window, cx| this.reconcile(window, cx));
        Self {
            owner,
            store,
            threads: HashMap::default(),
            drafts: Vec::new(),
            blocks: Vec::new(),
            block_layout: Vec::new(),
            locations: HashMap::default(),
            inputs: Vec::new(),
            resolving: Task::ready(()),
            pending_open: None,
            last_error: None,
            _subscriptions: subscriptions,
        }
    }

    fn add_draft(
        &mut self,
        file: PathBuf,
        content_id: String,
        anchor: Anchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parent = cx.weak_entity();
        let thread = cx.new(|cx| {
            InboxThread::new(
                parent,
                self.store.clone(),
                None,
                Some((file, content_id, anchor)),
                window,
                cx,
            )
        });
        self.drafts.push(thread.clone());
        self.install_blocks(window, cx);
        window.focus(&thread.read(cx).input.focus_handle(cx), cx);
    }

    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if store.error != self.last_error {
            self.last_error = store.error.clone();
            if let Some(error) = self.last_error.clone() {
                report_error(&self.owner, error, cx);
            }
        }
        let Some(editor) = self.owner.upgrade() else {
            return;
        };
        let rows = if InboxSettings::get_global(cx).enabled {
            self.store.read(cx).snapshot.clone()
        } else {
            Vec::new()
        };
        let show_resolved = self.store.read(cx).show_resolved;
        let buffers: Vec<_> = editor
            .read(cx)
            .buffer
            .read(cx)
            .all_buffers_iter()
            .filter_map(|buffer| {
                let buffer = buffer.read(cx);
                let file = buffer.file().and_then(|file| file.as_local())?;
                Some((file.abs_path(cx), buffer.snapshot()))
            })
            .collect();
        let mut visible = HashSet::default();
        let mut inputs = Vec::new();
        for row in rows {
            if row.thread.resolved && !show_resolved {
                continue;
            }
            let Some((_, buffer)) = buffers
                .iter()
                .find(|(path, _)| same_file(path, &row.thread.file))
            else {
                continue;
            };
            let sqid = row.thread.sqid.clone();
            visible.insert(sqid.clone());
            inputs.push((sqid.clone(), row.thread.content_id.clone(), buffer.text()));
            if let Some(thread) = self.threads.get(&sqid) {
                thread.update(cx, |thread, cx| thread.update_row(row, cx));
            } else {
                let parent = cx.weak_entity();
                let thread = cx.new(|cx| {
                    InboxThread::new(parent, self.store.clone(), Some(row), None, window, cx)
                });
                self.threads.insert(sqid, thread);
            }
        }
        self.threads.retain(|sqid, _| visible.contains(sqid));
        self.drafts.retain(|draft| draft.read(cx).draft.is_some());
        inputs.sort_by(|left, right| left.0.cmp(&right.0));
        if inputs != self.inputs {
            self.inputs = inputs.clone();
            self.locations.clear();
            self.resolving = cx.spawn_in(window, async move |this, cx| {
                let locations = cx
                    .background_spawn(async move {
                        inputs
                            .into_iter()
                            .map(|(sqid, anchor, text)| {
                                let location = anchors::locate_content_id(
                                    &text,
                                    &anchor,
                                    &LocateOptions::default(),
                                )
                                .unwrap_or(LocateResult::Invalid);
                                (sqid, location)
                            })
                            .collect()
                    })
                    .await;
                this.update_in(cx, |this, window, cx| {
                    this.locations = locations;
                    this.install_blocks(window, cx);
                })
                .log_err();
            });
        }
        self.install_blocks(window, cx);
    }

    fn install_blocks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.owner.upgrade() else {
            return;
        };
        let snapshot = editor.read(cx).buffer.read(cx).snapshot(cx);
        let mut projections = Vec::new();
        for (sqid, thread) in &self.threads {
            let Some(row) = &thread.read(cx).row else {
                continue;
            };
            let buffer = editor
                .read(cx)
                .buffer
                .read(cx)
                .all_buffers_iter()
                .find(|buffer| {
                    buffer
                        .read(cx)
                        .file()
                        .and_then(|file| file.as_local())
                        .is_some_and(|file| same_file(&file.abs_path(cx), &row.thread.file))
                });
            let Some(buffer) = buffer else { continue };
            let buffer = buffer.read(cx).snapshot();
            let location = self
                .locations
                .get(sqid)
                .cloned()
                .unwrap_or(LocateResult::Invalid);
            let (point, label) = location_placement(&buffer.text(), &location);
            let anchor = snapshot
                .anchor_in_excerpt(buffer.anchor_after(point))
                .or_else(|| {
                    snapshot
                        .excerpts_for_buffer(buffer.remote_id())
                        .next()
                        .and_then(|excerpt| snapshot.anchor_in_excerpt(excerpt.context.start))
                });
            let Some(anchor) = anchor else { continue };
            thread.update(cx, |thread, cx| {
                thread.location_label = label;
                if self.pending_open.as_ref() == Some(sqid) {
                    thread.expanded = true;
                }
                cx.notify();
            });
            projections.push((anchor, thread.clone()));
        }
        projections.sort_by(|left, right| left.0.cmp(&right.0, &snapshot));
        for draft in self
            .drafts
            .iter()
            .filter(|_| InboxSettings::get_global(cx).enabled)
        {
            if let Some((_, _, anchor)) = &draft.read(cx).draft {
                projections.push((*anchor, draft.clone()));
            }
        }
        let open = self
            .pending_open
            .as_ref()
            .filter(|sqid| self.locations.contains_key(*sqid))
            .cloned();
        if open.is_some() {
            self.pending_open = None;
        }
        let selected = open.and_then(|sqid| {
            projections
                .iter()
                .find(|(_, thread)| {
                    thread
                        .read(cx)
                        .row
                        .as_ref()
                        .is_some_and(|row| row.thread.sqid == sqid)
                })
                .map(|(anchor, _)| *anchor)
        });
        let layout = projections
            .iter()
            .map(|(anchor, thread)| {
                (
                    *anchor,
                    thread.entity_id(),
                    if thread.read(cx).expanded { 20 } else { 3 },
                )
            })
            .collect::<Vec<_>>();
        if selected.is_none()
            && layout.len() == self.block_layout.len()
            && layout.iter().zip(&self.block_layout).all(
                |((anchor, entity, height), (old_anchor, old_entity, old_height))| {
                    old_anchor.is_valid(&snapshot)
                        && anchor.cmp(old_anchor, &snapshot).is_eq()
                        && entity == old_entity
                        && height == old_height
                },
            )
        {
            return;
        }
        self.block_layout = layout;
        let blocks = projections
            .into_iter()
            .map(|(anchor, thread)| {
                let height = if thread.read(cx).expanded { 20 } else { 3 };
                BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(height),
                    style: BlockStyle::Sticky,
                    priority: 0,
                    render: Arc::new(move |_| thread.clone().into_any_element()),
                }
            })
            .collect::<Vec<_>>();
        let old_blocks = std::mem::take(&mut self.blocks).into_iter().collect();
        self.blocks = editor.update(cx, |editor, cx| {
            editor.remove_blocks(old_blocks, None, cx);
            if let Some(anchor) = selected {
                editor.change_selections(
                    crate::SelectionEffects::scroll(Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([anchor..anchor]),
                );
            }
            editor.insert_blocks(blocks, None, cx)
        });
    }
}

fn same_file(path: &std::path::Path, stored: &str) -> bool {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        path.replace('\\', "/")
            .eq_ignore_ascii_case(&stored.replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        path == stored
    }
}

fn location_placement(text: &str, location: &LocateResult) -> (Point, String) {
    match location {
        LocateResult::Matched {
            start, count, kind, ..
        } => {
            let row = start + count - 1;
            let lines = anchors::source_lines(text);
            let column = lines.get(row).map_or(0, |line| line.len());
            let label = match kind {
                MatchKind::Modified => " · selected text modified",
                MatchKind::Whitespace => " · whitespace match",
                _ => "",
            };
            (Point::new(row as u32, column as u32), label.into())
        }
        other => (
            Point::new(0, 0),
            format!(" · anchor unverified ({other:?}; file-level)"),
        ),
    }
}

struct InboxThread {
    parent: WeakEntity<InboxEditor>,
    store: Entity<InboxStore>,
    row: Option<ThreadWithMessages>,
    draft: Option<(PathBuf, String, Anchor)>,
    input: Entity<Editor>,
    markdown: HashMap<u64, (String, Entity<Markdown>)>,
    editing: Option<(u64, String)>,
    expanded: bool,
    busy: bool,
    error: Option<String>,
    location_label: String,
    _timestamp_task: Task<()>,
}

impl InboxThread {
    fn new(
        parent: WeakEntity<InboxEditor>,
        store: Entity<InboxStore>,
        row: Option<ThreadWithMessages>,
        draft: Option<(PathBuf, String, Anchor)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let thread = cx.weak_entity();
        let input = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 6, window, cx);
            editor.set_placeholder_text("Leave a comment", window, cx);
            editor.register_addon(ComposerAddon);
            editor
                .register_action({
                    let thread = thread.clone();
                    move |_: &Post, window, cx| {
                        thread
                            .update(cx, |thread, cx| thread.submit(window, cx))
                            .log_err();
                    }
                })
                .detach();
            editor
                .register_action(move |_: &CancelComposer, window, cx| {
                    thread
                        .update(cx, |thread, cx| {
                            if thread.busy {
                                return;
                            }
                            if thread.editing.is_some() {
                                thread.editing = None;
                                thread.input.update(cx, |input, cx| input.clear(window, cx));
                                cx.notify();
                            } else if thread.draft.is_some() {
                                thread.discard(window, cx);
                            } else {
                                thread.toggle(window, cx);
                            }
                        })
                        .log_err();
                })
                .detach();
            editor
        });
        #[cfg(not(any(test, feature = "test-support")))]
        let timestamp_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.expanded {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        #[cfg(any(test, feature = "test-support"))]
        let timestamp_task = Task::ready(());
        let expanded = draft.is_some();
        let mut this = Self {
            parent,
            store,
            row: None,
            draft,
            input,
            markdown: HashMap::default(),
            editing: None,
            expanded,
            busy: false,
            error: None,
            location_label: String::new(),
            _timestamp_task: timestamp_task,
        };
        if let Some(row) = row {
            this.update_row(row, cx);
        }
        this
    }

    fn update_row(&mut self, row: ThreadWithMessages, cx: &mut Context<Self>) {
        for message in &row.messages {
            if self
                .markdown
                .get(&message.id)
                .is_none_or(|(body, _)| body != &message.body)
            {
                let markdown = cx.new(|cx| {
                    Markdown::new_with_options(
                        message.body.clone().into(),
                        None,
                        None,
                        MarkdownOptions {
                            parse_html: false,
                            render_mermaid_diagrams: false,
                            ..Default::default()
                        },
                        cx,
                    )
                });
                self.markdown
                    .insert(message.id, (message.body.clone(), markdown));
            }
        }
        self.markdown
            .retain(|id, _| row.messages.iter().any(|message| message.id == *id));
        self.row = Some(row);
        cx.notify();
    }

    fn begin_edit(&mut self, message_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(message) = self.row.as_ref().and_then(|row| {
            row.messages
                .iter()
                .find(|message| message.id == message_id && message.source == Source::User)
        }) else {
            return;
        };
        let body = message.body.clone();
        self.editing = Some((message_id, body.clone()));
        self.input
            .update(cx, |input, cx| input.set_text(body, window, cx));
        self.expanded = true;
        let parent = self.parent.clone();
        window.defer(cx, move |window, cx| {
            parent
                .update(cx, |parent, cx| parent.install_blocks(window, cx))
                .log_err();
        });
        window.focus(&self.input.focus_handle(cx), cx);
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let body = self.input.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        let mutation = if let Some((file, content_id, _)) = &self.draft {
            Mutation::Create {
                file: file.to_string_lossy().into_owned(),
                content_id: content_id.clone(),
                body,
            }
        } else if let Some(row) = &self.row {
            if let Some((message_id, original_body)) = &self.editing {
                Mutation::Edit {
                    sqid: row.thread.sqid.clone(),
                    message_id: *message_id,
                    original_body: original_body.clone(),
                    body,
                }
            } else {
                Mutation::Reply {
                    sqid: row.thread.sqid.clone(),
                    body,
                }
            }
        } else {
            return;
        };
        self.busy = true;
        self.error = None;
        let task = self
            .store
            .update(cx, |store, cx| store.mutate(mutation, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(result) => {
                        this.draft = None;
                        this.editing = None;
                        this.error = result.refresh_error;
                        if let Some(sqid) = result.created_sqid {
                            this.parent
                                .update(cx, |parent, _| {
                                    parent.pending_open = Some(sqid);
                                })
                                .log_err();
                        }
                        this.input.update(cx, |input, cx| input.clear(window, cx));
                    }
                    Err(error) => this.error = Some(error),
                }
                let parent = this.parent.clone();
                window.defer(cx, move |window, cx| {
                    parent
                        .update(cx, |parent, cx| parent.reconcile(window, cx))
                        .log_err();
                });
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn resolve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(row) = &self.row else { return };
        let mutation = Mutation::Resolve {
            sqid: row.thread.sqid.clone(),
            resolved: !row.thread.resolved,
        };
        self.busy = true;
        let task = self
            .store
            .update(cx, |store, cx| store.mutate(mutation, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, _, cx| {
                this.busy = false;
                this.error = match result {
                    Ok(result) => result.refresh_error,
                    Err(error) => Some(error),
                };
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        let restore_focus = !self.expanded;
        let parent = self.parent.clone();
        window.defer(cx, move |window, cx| {
            parent
                .update(cx, |parent, cx| {
                    parent.install_blocks(window, cx);
                    if restore_focus && let Some(editor) = parent.owner.upgrade() {
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                })
                .log_err();
        });
        cx.notify();
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.draft = None;
        let parent = self.parent.clone();
        window.defer(cx, move |window, cx| {
            parent
                .update(cx, |parent, cx| {
                    parent.drafts.retain(|draft| draft.read(cx).draft.is_some());
                    parent.install_blocks(window, cx);
                    if let Some(editor) = parent.owner.upgrade() {
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                })
                .log_err();
        });
    }
}

impl Render for InboxThread {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .row
            .as_ref()
            .map_or("New Pi Inbox thread".into(), |row| {
                format!(
                    "{}{}",
                    if row.thread.resolved {
                        "Resolved"
                    } else {
                        "Open"
                    },
                    self.location_label
                )
            });
        v_flex()
            .w_full()
            .h_full()
            .px_2()
            .py_1()
            .gap_1()
            .bg(cx.theme().colors().editor_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Button::new("inbox-expand", title).on_click(
                                cx.listener(|this, _, window, cx| this.toggle(window, cx)),
                            ))
                            .when_some(self.row.as_ref(), |element, row| {
                                element.child(
                                    Label::new(row.thread.sqid.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .when(self.row.is_some(), |element| {
                                element.child(
                                    Button::new(
                                        "inbox-resolve",
                                        if self.row.as_ref().is_some_and(|row| row.thread.resolved)
                                        {
                                            "Reopen"
                                        } else {
                                            "Resolve"
                                        },
                                    )
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.resolve(window, cx)),
                                    ),
                                )
                            })
                            .when(self.draft.is_some(), |element| {
                                element.child(
                                    IconButton::new("inbox-discard", IconName::Close)
                                        .disabled(self.busy)
                                        .tooltip(Tooltip::text("Discard unposted draft"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.discard(window, cx)
                                        })),
                                )
                            }),
                    ),
            )
            .when(self.expanded, |element| {
                element
                    .child(
                        v_flex()
                            .id("inbox-messages")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .gap_2()
                            .children(self.row.iter().flat_map(|row| row.messages.iter()).map(
                                |message| {
                                    let message_id = message.id;
                                    let source = message.source;
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            h_flex()
                                                .w_full()
                                                .gap_2()
                                                .child(
                                                    Label::new(if source == Source::Agent {
                                                        "Agent"
                                                    } else {
                                                        "You"
                                                    })
                                                    .size(LabelSize::Small),
                                                )
                                                .child(
                                                    div()
                                                        .id(("inbox-message-time", message_id))
                                                        .tooltip(Tooltip::text(
                                                            message.created_at.clone(),
                                                        ))
                                                        .child(
                                                            Label::new(compact_timestamp(
                                                                &message.created_at,
                                                                OffsetDateTime::now_utc(),
                                                            ))
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                        ),
                                                )
                                                .child(div().flex_1())
                                                .when(source == Source::User, |element| {
                                                    element.child(
                                                        Button::new(
                                                            ("inbox-edit", message_id),
                                                            "Edit",
                                                        )
                                                        .disabled(self.busy)
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                this.begin_edit(
                                                                    message_id, window, cx,
                                                                );
                                                            },
                                                        )),
                                                    )
                                                }),
                                        )
                                        .when_some(
                                            self.markdown.get(&message_id),
                                            |element, (_, markdown)| {
                                                element.child(MarkdownElement::new(
                                                    markdown.clone(),
                                                    crate::hover_markdown_style(window, cx),
                                                ))
                                            },
                                        )
                                },
                            ))
                            .when_some(self.error.clone(), |element, error| {
                                element.child(Label::new(error).color(Color::Error))
                            }),
                    )
                    .child(self.input.clone())
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(
                                    "inbox-post",
                                    if self.editing.is_some() {
                                        "Save Edit"
                                    } else {
                                        "Post to Pi Inbox"
                                    },
                                )
                                .disabled(self.busy)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                ),
                            )
                            .when(self.editing.is_some(), |element| {
                                element.child(
                                    Button::new("inbox-cancel-edit", "Cancel Edit")
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.editing = None;
                                            this.input
                                                .update(cx, |input, cx| input.clear(window, cx));
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
            })
    }
}

fn compact_timestamp(timestamp: &str, reference: OffsetDateTime) -> String {
    let Ok(timestamp) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return timestamp.to_string();
    };
    let seconds = (reference - timestamp).whole_seconds().max(0);
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d {}h ago", seconds / 86400, seconds % 86400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_timestamps_use_seconds_minutes_hours_and_days() -> anyhow::Result<()> {
        let timestamp = "2026-10-07T00:00:00.000Z";
        let original = OffsetDateTime::parse(timestamp, &Rfc3339)?;
        for (seconds, expected) in [
            (-1, "0s ago"),
            (0, "0s ago"),
            (59, "59s ago"),
            (60, "1m ago"),
            (3599, "59m ago"),
            (3600, "1h ago"),
            (86399, "23h ago"),
            (86400, "1d 0h ago"),
            (90000, "1d 1h ago"),
            (180000, "2d 2h ago"),
        ] {
            assert_eq!(
                compact_timestamp(timestamp, original + time::Duration::seconds(seconds)),
                expected
            );
        }
        assert_eq!(compact_timestamp("invalid", original), "invalid");
        Ok(())
    }

    #[test]
    fn unmatched_locations_stay_file_level() {
        for location in [
            LocateResult::Missing,
            LocateResult::Ambiguous,
            LocateResult::Invalid,
            LocateResult::Limit,
        ] {
            let (point, label) = location_placement("hello\nworld\n", &location);
            assert_eq!(point, Point::new(0, 0));
            assert!(label.contains("file-level"));
        }
    }

    #[test]
    fn modified_and_whitespace_locations_are_labeled() {
        for (kind, label) in [
            (MatchKind::Modified, "selected text modified"),
            (MatchKind::Whitespace, "whitespace match"),
        ] {
            let location = LocateResult::Matched {
                start: 1,
                count: 1,
                kind,
                moved: true,
            };
            let (point, description) = location_placement("first\nλ\n", &location);
            assert_eq!(point, Point::new(1, 2));
            assert!(description.contains(label));
        }
    }
}
