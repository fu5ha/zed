use git::jj::JjBookmark;
use gpui::{App, DismissEvent, Entity, Subscription, Task, Window};
use picker::{Picker, PickerDelegate};
use project::jj_store::JjStore;
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};

#[derive(Clone)]
pub struct JjBookmarkSelection {
    pub bookmark: JjBookmark,
    pub commit_id: String,
}

pub type BookmarkSelectionHandler = Box<dyn Fn(JjBookmarkSelection, &mut Window, &mut App)>;

struct BookmarkRow {
    bookmark: JjBookmark,
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
    cx.new(|cx| new(store, on_select, true, window, cx))
}

pub fn modal(
    store: Entity<JjStore>,
    window: &mut Window,
    cx: &mut Context<Picker<JjBookmarkDelegate>>,
) -> Picker<JjBookmarkDelegate> {
    new(store, None, false, window, cx)
}

fn new(
    store: Entity<JjStore>,
    on_select: Option<BookmarkSelectionHandler>,
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
    _subscription: Subscription,
}

impl JjBookmarkDelegate {
    fn refresh(&mut self, cx: &App) {
        let selected_target = self
            .matches
            .get(self.selected_index)
            .and_then(|index| self.rows.get(*index))
            .map(|row| {
                (
                    row.bookmark.name.clone(),
                    row.bookmark.remote.clone(),
                    row.commit_id.clone(),
                )
            });
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
                        bookmark: bookmark.clone(),
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
                            bookmark: bookmark.clone(),
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
                .remote
                .is_some()
                .cmp(&right.bookmark.remote.is_some())
                .then_with(|| left.label.cmp(&right.label))
                .then_with(|| left.commit_id.cmp(&right.commit_id))
        });
        self.filter();
        self.selected_index = selected_target
            .and_then(|(name, remote, commit_id)| {
                self.matches.iter().position(|index| {
                    self.rows.get(*index).is_some_and(|row| {
                        row.bookmark.name == name
                            && row.bookmark.remote == remote
                            && row.commit_id == commit_id
                    })
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
        if self.on_select.is_some() {
            "Select a jj bookmark target…".into()
        } else {
            "Browse jj bookmarks (read-only)…".into()
        }
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(if self.rows.is_empty() {
            self.empty_message.clone().into()
        } else {
            "No matching bookmarks".into()
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
            .is_some_and(|row| self.on_select.is_none() || row.commit_id.is_some())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(row) = self
            .matches
            .get(self.selected_index)
            .and_then(|index| self.rows.get(*index))
        else {
            return;
        };
        if let Some(handler) = &self.on_select {
            let Some(commit_id) = row.commit_id.clone() else {
                return;
            };
            handler(
                JjBookmarkSelection {
                    bookmark: row.bookmark.clone(),
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
        let unavailable_target = self.on_select.is_some() && row.commit_id.is_none();
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
