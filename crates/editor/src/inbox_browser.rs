use crate::{
    Editor,
    inbox::{Refresh, ShowThreads, ToggleResolved},
    inbox_settings::InboxSettings,
    inbox_store::InboxStore,
};
use gpui::{
    App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle, Styled, Subscription,
    WeakEntity, Window, div,
};
use settings::{Settings, SettingsStore};
use std::path::PathBuf;
use ui::{Button, ButtonCommon, Clickable, ListItem, Tooltip, prelude::*};
use workspace::{
    HideStatusItem, ModalView, OpenOptions, StatusItemView, Toast, Workspace, item::ItemHandle,
    notifications::NotificationId,
};

pub(crate) fn init(cx: &mut App) {
    cx.observe_new::<Workspace>(|workspace, window, cx| {
        workspace.register_action(|workspace, _: &ShowThreads, window, cx| {
            let workspace_handle = workspace.weak_handle();
            workspace.toggle_modal(window, cx, move |_, cx| {
                InboxBrowser::new(workspace_handle, cx)
            });
        });
        workspace.register_action(|_, _: &ToggleResolved, _, cx| {
            InboxStore::global(cx).update(cx, |store, cx| store.toggle_resolved(cx));
        });
        workspace.register_action(|workspace, _: &Refresh, window, cx| {
            let task = InboxStore::global(cx).update(cx, |store, cx| store.refresh(cx));
            let workspace = workspace.weak_handle();
            window
                .spawn(cx, async move |cx| {
                    if let Err(error) = task.await {
                        workspace.update(cx, |workspace, cx| show_error(workspace, error, cx))?;
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
        });
        if let Some(window) = window {
            let workspace_handle = workspace.weak_handle();
            let indicator = cx.new(|cx| InboxIndicator::new(workspace_handle, cx));
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_right_item(indicator, window, cx);
            });
        }
    })
    .detach();
}

fn show_error(workspace: &mut Workspace, error: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<InboxBrowser>(), error),
        cx,
    );
}

struct InboxIndicator {
    store: Entity<InboxStore>,
    workspace: WeakEntity<Workspace>,
    _subscription: Subscription,
    _settings_subscription: Subscription,
}

impl InboxIndicator {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let store = InboxStore::global(cx);
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        let settings_subscription = cx.observe_global::<SettingsStore>(|_, cx| cx.notify());
        Self {
            store,
            workspace,
            _subscription: subscription,
            _settings_subscription: settings_subscription,
        }
    }
}

impl StatusItemView for InboxIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings.pi_inbox.get_or_insert_default().show_status_button = Some(false);
        }))
    }
}

impl Render for InboxIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = InboxSettings::get_global(cx);
        let visible = settings.enabled && settings.show_status_button;
        let store = self.store.read(cx);
        let count = store
            .snapshot
            .iter()
            .filter(|entry| !entry.thread.resolved)
            .count();
        let label = if store.error.is_some() {
            format!("Inbox: {count} !")
        } else {
            format!("Inbox: {count}")
        };
        div().when(visible, |this| {
            this.child(
                Button::new("pi-inbox-status", label)
                    .label_size(LabelSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("Show Pi Inbox Threads", &ShowThreads, cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(workspace) = this.workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                let workspace_handle = workspace.weak_handle();
                                workspace.toggle_modal(window, cx, move |_, cx| {
                                    InboxBrowser::new(workspace_handle, cx)
                                });
                            });
                        }
                    })),
            )
        })
    }
}

struct InboxBrowser {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    store: Entity<InboxStore>,
    selected_index: usize,
    scroll_handle: ScrollHandle,
    _subscription: Subscription,
}

impl InboxBrowser {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let store = InboxStore::global(cx);
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            store,
            selected_index: 0,
            scroll_handle: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    fn select(&mut self, next: bool, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let count = store
            .snapshot
            .iter()
            .filter(|entry| store.show_resolved || !entry.thread.resolved)
            .count();
        if count == 0 {
            return;
        }
        self.selected_index = self.selected_index.min(count - 1);
        self.selected_index = if next {
            (self.selected_index + 1) % count
        } else if self.selected_index == 0 {
            count - 1
        } else {
            self.selected_index - 1
        };
        self.scroll_handle.scroll_to_item(self.selected_index);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let target = store
            .snapshot
            .iter()
            .filter(|entry| store.show_resolved || !entry.thread.resolved)
            .nth(self.selected_index)
            .map(|entry| (PathBuf::from(&entry.thread.file), entry.thread.sqid.clone()));
        if let Some((path, sqid)) = target {
            self.open_thread(path, sqid, window, cx);
        }
    }

    fn open_thread(
        &mut self,
        path: PathBuf,
        sqid: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        window
            .spawn(cx, async move |cx| {
                let result: anyhow::Result<()> = async {
                    let file_exists = cx
                        .background_spawn({
                            let path = path.clone();
                            async move { path.is_absolute() && path.is_file() }
                        })
                        .await;
                    anyhow::ensure!(
                        file_exists,
                        "Inbox file no longer exists: {}",
                        path.display()
                    );
                    let item = workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
                        })?
                        .await?;
                    let editor = item.downcast::<Editor>().ok_or_else(|| {
                        anyhow::anyhow!("Inbox file cannot be opened as a text editor")
                    })?;
                    editor.update_in(cx, |editor, window, cx| {
                        editor.show_inbox_thread(&sqid, window, cx);
                    })?;
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    workspace.update(cx, |workspace, cx| {
                        show_error(workspace, error.to_string(), cx)
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }
}

impl Focusable for InboxBrowser {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl EventEmitter<DismissEvent> for InboxBrowser {}
impl ModalView for InboxBrowser {}

fn excerpt(body: &str) -> String {
    let mut characters = body.chars();
    let mut excerpt: String = characters
        .by_ref()
        .take(240)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if characters.next().is_some() {
        excerpt.push('…');
    }
    excerpt
}

impl Render for InboxBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let show_resolved = store.show_resolved;
        let error = store.error.clone();
        let count = store
            .snapshot
            .iter()
            .filter(|entry| show_resolved || !entry.thread.resolved)
            .count();
        self.selected_index = self.selected_index.min(count.saturating_sub(1));
        let rows = store
            .snapshot
            .iter()
            .filter(|entry| show_resolved || !entry.thread.resolved)
            .enumerate()
            .map(|(index, entry)| {
                let path = PathBuf::from(&entry.thread.file);
                let sqid = entry.thread.sqid.clone();
                let first = entry.messages.first().map(|message| excerpt(&message.body));
                let latest = (entry.messages.len() > 1)
                    .then(|| entry.messages.last().map(|message| excerpt(&message.body)))
                    .flatten();
                ListItem::new(("pi-inbox-thread", index))
                    .toggle_state(index == self.selected_index)
                    .child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1()
                            .child(
                                Label::new(format!(
                                    "{} · {}{}",
                                    entry.thread.file,
                                    entry.thread.sqid,
                                    if entry.thread.resolved {
                                        " · Resolved"
                                    } else {
                                        ""
                                    }
                                ))
                                .single_line()
                                .truncate(),
                            )
                            .when_some(first, |this, body| {
                                this.child(Label::new(body).single_line().truncate())
                            })
                            .when_some(latest, |this, body| {
                                this.child(
                                    Label::new(body)
                                        .single_line()
                                        .truncate()
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_thread(path.clone(), sqid.clone(), window, cx);
                    }))
            })
            .collect::<Vec<_>>();
        let empty = rows.is_empty();
        v_flex()
            .id("pi-inbox-browser")
            .w(rems(48.))
            .elevation_3(cx)
            .p_3()
            .gap_3()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.select(true, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.select(false, cx)))
            .on_action(cx.listener(Self::confirm))
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Pi Inbox Threads"))
                    .child(div().flex_1())
                    .child(
                        Button::new(
                            "inbox-resolved",
                            if show_resolved {
                                "Hide Resolved"
                            } else {
                                "Show Resolved"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |store, cx| store.toggle_resolved(cx));
                        })),
                    )
                    .child(
                        Button::new("inbox-refresh", "Refresh").on_click(cx.listener(
                            |this, _, window, cx| {
                                let task = this.store.update(cx, |store, cx| store.refresh(cx));
                                window
                                    .spawn(cx, async move |_| {
                                        task.await.map_err(|error| anyhow::anyhow!(error))
                                    })
                                    .detach_and_log_err(cx);
                            },
                        )),
                    )
                    .child(
                        Button::new("inbox-close", "Close")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(Label::new(error).color(Color::Error))
            })
            .child(
                v_flex()
                    .id("pi-inbox-thread-list")
                    .max_h(rems(30.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(rows)
                    .when(empty, |this| {
                        this.child(Label::new("No threads").color(Color::Muted))
                    }),
            )
    }
}
