use git::jj::{JjBookmark, JjRevision};
use gpui::{App, DismissEvent, Entity, Subscription, Task, WeakEntity, Window};
use picker::{Picker, PickerDelegate};
use project::jj_store::JjStore;
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{Workspace, notifications::DetachAndPromptErr};

#[derive(Clone)]
pub struct JjBookmarkSelection {
    pub bookmark: JjBookmark,
    pub commit_id: String,
}

pub type BookmarkSelectionHandler = Box<dyn Fn(JjBookmarkSelection, &mut Window, &mut App)>;

struct BookmarkRow {
    bookmark: Option<JjBookmark>,
    commit_id: Option<String>,
    label: String,
    detail: String,
}

pub fn popover(
    store: Entity<JjStore>,
    on_select: Option<BookmarkSelectionHandler>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Picker<JjBookmarkDelegate>> {
    cx.new(|cx| new(store, on_select, None, true, window, cx))
}

pub fn switch_popover(
    store: Entity<JjStore>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Picker<JjBookmarkDelegate>> {
    cx.new(|cx| new(store, None, Some(workspace), true, window, cx))
}

pub fn modal(
    store: Entity<JjStore>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<Picker<JjBookmarkDelegate>>,
) -> Picker<JjBookmarkDelegate> {
    new(store, None, Some(workspace), false, window, cx)
}

pub fn selection_modal(
    store: Entity<JjStore>,
    on_select: BookmarkSelectionHandler,
    window: &mut Window,
    cx: &mut Context<Picker<JjBookmarkDelegate>>,
) -> Picker<JjBookmarkDelegate> {
    new(store, Some(on_select), None, false, window, cx)
}

fn new(
    store: Entity<JjStore>,
    on_select: Option<BookmarkSelectionHandler>,
    workspace: Option<WeakEntity<Workspace>>,
    is_popover: bool,
    window: &mut Window,
    cx: &mut Context<Picker<JjBookmarkDelegate>>,
) -> Picker<JjBookmarkDelegate> {
    let subscription = cx.observe(&store, |picker: &mut Picker<JjBookmarkDelegate>, _, cx| {
        picker.delegate.refresh(cx);
        cx.notify();
    });
    let mut delegate = JjBookmarkDelegate {
        store,
        rows: Vec::new(),
        matches: Vec::new(),
        query: String::new(),
        selected_index: 0,
        empty_message: String::new(),
        on_select,
        workspace,
        _subscription: subscription,
    };
    delegate.refresh(cx);
    let picker = Picker::uniform_list(delegate, window, cx)
        .initial_width(rems(34.))
        .show_scrollbar(true);
    if is_popover { picker.popover() } else { picker }
}

pub struct JjBookmarkDelegate {
    store: Entity<JjStore>,
    rows: Vec<BookmarkRow>,
    matches: Vec<usize>,
    query: String,
    selected_index: usize,
    empty_message: String,
    on_select: Option<BookmarkSelectionHandler>,
    workspace: Option<WeakEntity<Workspace>>,
    _subscription: Subscription,
}

impl JjBookmarkDelegate {
    fn refresh(&mut self, cx: &App) {
        let selected_target = self
            .matches
            .get(self.selected_index)
            .and_then(|index| self.rows.get(*index))
            .map(|row| (row.label.clone(), row.commit_id.clone()));
        let state = self.store.read(cx);
        self.rows.clear();
        self.empty_message = state
            .error
            .clone()
            .or_else(|| {
                state
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.bookmark_error.clone())
            })
            .unwrap_or_else(|| {
                if state.snapshot.is_some() {
                    "No bookmarks".into()
                } else {
                    "Loading bookmarks".into()
                }
            });
        if let Some(snapshot) = &state.snapshot {
            for bookmark in snapshot.bookmarks.iter() {
                let label = match &bookmark.remote {
                    Some(remote) => format!("{}@{remote}", bookmark.name),
                    None => bookmark.name.clone(),
                };
                let kind = if bookmark.remote.is_some() {
                    "Remote"
                } else {
                    "Local"
                };
                if bookmark.targets.is_empty() {
                    self.rows.push(BookmarkRow {
                        bookmark: Some(bookmark.clone()),
                        commit_id: None,
                        label,
                        detail: if bookmark.conflict {
                            format!("{kind} · conflicted · no target")
                        } else {
                            format!("{kind} · no target")
                        },
                    });
                } else {
                    for target in &bookmark.targets {
                        let short_target = target.chars().take(12).collect::<String>();
                        let current = if target == &snapshot.revision.commit_id {
                            " · at current change"
                        } else {
                            ""
                        };
                        let conflict = if bookmark.conflict {
                            " · conflicted target"
                        } else {
                            ""
                        };
                        self.rows.push(BookmarkRow {
                            bookmark: Some(bookmark.clone()),
                            commit_id: Some(target.clone()),
                            label: label.clone(),
                            detail: format!("{kind} · {short_target}{conflict}{current}"),
                        });
                    }
                }
            }
        }
        self.rows.sort_by(|left, right| {
            left.bookmark
                .as_ref()
                .is_some_and(|bookmark| bookmark.remote.is_some())
                .cmp(
                    &right
                        .bookmark
                        .as_ref()
                        .is_some_and(|bookmark| bookmark.remote.is_some()),
                )
                .then_with(|| left.label.cmp(&right.label))
                .then_with(|| left.commit_id.cmp(&right.commit_id))
        });
        if self.workspace.is_some() {
            if let Some(snapshot) = &state.snapshot {
                let mut changes = snapshot.nearby_changes.iter().collect::<Vec<_>>();
                changes.retain(|revision| revision.commit_id != snapshot.revision.commit_id);
                changes.insert(0, &snapshot.revision);
                let mut rows = changes
                    .into_iter()
                    .map(|revision| {
                        let change_id = revision.change_id.chars().take(8).collect::<String>();
                        BookmarkRow {
                            bookmark: None,
                            commit_id: Some(revision.commit_id.clone()),
                            label: format!("{change_id} · {}", change_description(revision)),
                            detail: change_detail(
                                revision,
                                &snapshot.revision,
                                &snapshot.nearby_changes,
                            ),
                        }
                    })
                    .collect::<Vec<_>>();
                rows.append(&mut self.rows);
                if let Some(error) = &snapshot.bookmark_error {
                    rows.push(BookmarkRow {
                        bookmark: None,
                        commit_id: None,
                        label: "Bookmarks unavailable".into(),
                        detail: error.clone(),
                    });
                }
                self.rows = rows;
                if let Some(error) = &snapshot.change_error {
                    self.rows.insert(
                        0,
                        BookmarkRow {
                            bookmark: None,
                            commit_id: None,
                            label: "Nearby changes unavailable".into(),
                            detail: error.clone(),
                        },
                    );
                }
            }
        }
        self.filter();
        self.selected_index = selected_target
            .and_then(|(label, commit_id)| {
                self.matches.iter().position(|index| {
                    self.rows
                        .get(*index)
                        .is_some_and(|row| row.label == label && row.commit_id == commit_id)
                })
            })
            .unwrap_or(0);
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.matches = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.label.to_lowercase().contains(&query)
                    || row.detail.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.matches.len().saturating_sub(1));
    }
}

impl PickerDelegate for JjBookmarkDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "jj bookmarks"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index.min(self.matches.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        if self.workspace.is_some() {
            "Switch jj change or bookmark…".into()
        } else if self.on_select.is_some() {
            "Select a jj bookmark target…".into()
        } else {
            "Browse jj bookmarks (read-only)…".into()
        }
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(if self.rows.is_empty() {
            self.empty_message.clone().into()
        } else {
            "No matching changes or bookmarks".into()
        })
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query;
        self.filter();
        cx.notify();
        Task::ready(())
    }

    fn can_select(
        &self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> bool {
        self.matches
            .get(index)
            .and_then(|index| self.rows.get(*index))
            .is_some_and(|row| row.commit_id.is_some())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(row) = self
            .matches
            .get(self.selected_index)
            .and_then(|index| self.rows.get(*index))
        else {
            return;
        };
        if let Some(workspace) = &self.workspace {
            let Some(commit_id) = row.commit_id.clone() else {
                return;
            };
            if self
                .store
                .read(cx)
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.revision.commit_id == commit_id)
            {
                cx.emit(DismissEvent);
                return;
            }
            if !ensure_saved_buffers(workspace, cx) {
                return;
            }
            self.store
                .update(cx, |store, cx| store.edit_change(commit_id, cx))
                .detach_and_prompt_err("Failed to switch jj change", window, cx, |_, _, _| None);
        } else if let Some(handler) = &self.on_select {
            let Some(commit_id) = row.commit_id.clone() else {
                return;
            };
            let Some(bookmark) = row.bookmark.clone() else {
                return;
            };
            handler(
                JjBookmarkSelection {
                    bookmark,
                    commit_id,
                },
                window,
                cx,
            );
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = self.rows.get(*self.matches.get(index)?)?;
        let unavailable_target = row.commit_id.is_none();
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(
                            Label::new(row.label.clone())
                                .when(unavailable_target, |label| label.color(Color::Muted)),
                        )
                        .child(
                            Label::new(row.detail.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                ),
        )
    }
}

pub(crate) fn change_description(revision: &JjRevision) -> String {
    let description = revision
        .description
        .lines()
        .next()
        .filter(|description| !description.is_empty())
        .unwrap_or("(no description)");
    if revision.empty {
        format!("(empty) {description}")
    } else {
        description.to_owned()
    }
}

fn change_detail(revision: &JjRevision, current: &JjRevision, nearby: &[JjRevision]) -> String {
    if revision.commit_id == current.commit_id {
        return "Current change".into();
    }
    fn ancestor_distance(start: &JjRevision, target: &str, nearby: &[JjRevision]) -> Option<usize> {
        let mut parents = start.parents.iter().map(String::as_str).collect::<Vec<_>>();
        for distance in 1..=2 {
            if parents.contains(&target) {
                return Some(distance);
            }
            parents = parents
                .into_iter()
                .filter_map(|parent| nearby.iter().find(|revision| revision.commit_id == parent))
                .flat_map(|revision| revision.parents.iter().map(String::as_str))
                .collect();
        }
        None
    }
    if let Some(distance) = ancestor_distance(current, &revision.commit_id, nearby) {
        format!("Ancestor · {distance} back")
    } else if let Some(distance) = ancestor_distance(revision, &current.commit_id, nearby) {
        format!("Descendant · {distance} ahead")
    } else {
        "Recent change".into()
    }
}

pub(crate) fn ensure_saved_buffers(workspace: &WeakEntity<Workspace>, cx: &mut App) -> bool {
    workspace
        .update(cx, |workspace, cx| {
            if workspace
                .project()
                .read(cx)
                .dirty_buffers(cx)
                .next()
                .is_some()
            {
                workspace.show_error(
                    "Save or discard unsaved files before changing the active jj change",
                    cx,
                );
                false
            } else {
                true
            }
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_empty_changes_independently_of_description() {
        for (description, empty, expected) in [
            ("", false, "(no description)"),
            ("", true, "(empty) (no description)"),
            ("A change\nMore details", false, "A change"),
            ("A change\nMore details", true, "(empty) A change"),
            ("\nMore details", true, "(empty) (no description)"),
        ] {
            let revision = JjRevision {
                commit_id: String::new(),
                change_id: String::new(),
                description: description.into(),
                empty,
                parents: Vec::new(),
            };
            assert_eq!(change_description(&revision), expected);
        }
    }

    #[test]
    fn labels_changes_relative_to_working_copy() {
        let revision = |commit_id: &str, parents: &[&str]| JjRevision {
            commit_id: commit_id.into(),
            change_id: commit_id.into(),
            description: String::new(),
            empty: false,
            parents: parents.iter().map(|parent| (*parent).into()).collect(),
        };
        let grandparent = revision("grandparent", &[]);
        let parent = revision("parent", &["grandparent"]);
        let current = revision("current", &["parent"]);
        let child = revision("child", &["current"]);
        let grandchild = revision("grandchild", &["child"]);
        let unrelated = revision("unrelated", &[]);
        let nearby = vec![
            grandparent.clone(),
            parent.clone(),
            current.clone(),
            child.clone(),
            grandchild.clone(),
        ];
        assert_eq!(change_detail(&current, &current, &nearby), "Current change");
        assert_eq!(
            change_detail(&parent, &current, &nearby),
            "Ancestor · 1 back"
        );
        assert_eq!(
            change_detail(&grandparent, &current, &nearby),
            "Ancestor · 2 back"
        );
        assert_eq!(
            change_detail(&child, &current, &nearby),
            "Descendant · 1 ahead"
        );
        assert_eq!(
            change_detail(&grandchild, &current, &nearby),
            "Descendant · 2 ahead"
        );
        assert_eq!(
            change_detail(&unrelated, &current, &nearby),
            "Recent change"
        );
    }
}
