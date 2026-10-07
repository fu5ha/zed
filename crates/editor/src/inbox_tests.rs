use super::*;
use crate::{ToPoint as _, editor_tests::init_test, test::editor_test_context::EditorTestContext};
use gpui::TestAppContext;
use inbx::models::{Message, Thread};

fn reload_keymaps(cx: &mut App) {
    cx.clear_key_bindings();
    for asset in [
        settings::DEFAULT_KEYMAP_PATH,
        settings::VIM_KEYMAP_PATH,
        settings::SPECIFIC_OVERRIDES_KEYMAP_PATH,
    ] {
        let bindings = settings::KeymapFile::load_asset_allow_partial_failure(asset, cx)
            .expect("Valid shipped keymap");
        cx.bind_keys(bindings);
    }
}

async fn setup(
    cx: &mut TestAppContext,
    text: &str,
) -> (
    EditorTestContext,
    Entity<InboxStore>,
    Entity<InboxEditor>,
    String,
) {
    init_test(cx, |_| {});
    cx.update(reload_keymaps);
    let mut context = EditorTestContext::new(cx).await;
    context.set_state(text);
    let store = context.cx.update(|_, cx| InboxStore::test(cx));
    let state = context.update_editor(|editor, window, cx| {
        let owner = cx.entity().downgrade();
        let state = cx.new(|cx| InboxEditor::new(owner, store.clone(), window, cx));
        editor.register_addon(InboxAddon(state.clone()));
        state
    });
    let file = context.buffer(|buffer, cx| {
        buffer
            .file()
            .and_then(|file| file.as_local())
            .map(|file| file.abs_path(cx).to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    assert!(!file.is_empty());
    (context, store, state, file)
}

fn row(file: String, content_id: String) -> ThreadWithMessages {
    ThreadWithMessages {
        thread: Thread {
            id: 1,
            sqid: "test-thread".into(),
            file,
            content_id,
            resolved: false,
            created_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
            jj_change_id: None,
            git_commit: None,
        },
        messages: vec![message(1, Source::User, "Original question")],
    }
}

fn message(id: u64, source: Source, body: &str) -> Message {
    Message {
        thread_id: 1,
        id,
        source,
        body: body.into(),
        created_at: "2026-01-01".into(),
        updated_at: "2026-01-01".into(),
    }
}

fn publish(context: &mut EditorTestContext, store: &Entity<InboxStore>, row: ThreadWithMessages) {
    store.update(&mut context.cx, |store, cx| {
        store.snapshot = vec![row];
        cx.notify();
    });
    context.run_until_parked();
}

#[gpui::test]
async fn inbox_edits_target_individual_user_messages(cx: &mut TestAppContext) {
    let (mut context, store, state, file) = setup(cx, "ˇselected\n").await;
    let content_id =
        anchors::create_content_id("selected\n", 1, 1, 3).expect("Valid source anchor");
    let mut snapshot = row(file, content_id);
    snapshot
        .messages
        .push(message(2, Source::Agent, "Agent reply"));
    snapshot
        .messages
        .push(message(3, Source::User, "Follow-up question"));
    publish(&mut context, &store, snapshot);
    let thread = state.read_with(&context.cx, |state, _| {
        state
            .threads
            .get("test-thread")
            .expect("Published thread")
            .clone()
    });
    for (message_id, body) in [(1, "Original question"), (3, "Follow-up question")] {
        thread.update_in(&mut context.cx, |thread, window, cx| {
            thread.begin_edit(message_id, window, cx);
            assert_eq!(thread.editing, Some((message_id, body.into())));
            assert_eq!(thread.input.read(cx).text(cx), body);
            thread.begin_edit(2, window, cx);
            assert_eq!(thread.editing, Some((message_id, body.into())));
            assert_eq!(thread.input.read(cx).text(cx), body);
        });
        context.run_until_parked();
    }
}

#[gpui::test]
async fn inbox_draft_selects_complete_lines(cx: &mut TestAppContext) {
    let (mut context, _, state, file) =
        setup(cx, "before\n«selected one\nselected two\nˇ»after\n").await;
    context.update_editor(|editor, window, cx| {
        assert!(can_comment(editor, cx));
        editor.add_inbox_thread_at_selection(window, cx);
    });
    context.run_until_parked();
    state.read_with(&context.cx, |state, cx| {
        assert_eq!(state.drafts.len(), 1);
        assert_eq!(state.blocks.len(), 1);
        for draft in &state.drafts {
            let draft = draft.read(cx);
            assert!(draft.expanded);
            let Some((draft_file, content_id, anchor)) = &draft.draft else {
                panic!("Expected an unposted draft");
            };
            assert_eq!(draft_file.to_string_lossy(), file);
            let snapshot = context.editor.read(cx).buffer.read(cx).snapshot(cx);
            assert_eq!(anchor.to_point(&snapshot), Point::new(2, 12));
            assert!(matches!(
                anchors::locate_content_id(&snapshot.text(), content_id, &LocateOptions::default()),
                Ok(LocateResult::Matched {
                    start: 1,
                    count: 2,
                    ..
                })
            ));
        }
    });
}

#[gpui::test]
async fn inbox_refresh_preserves_composers_and_relocates_blocks(cx: &mut TestAppContext) {
    let (mut context, store, state, file) = setup(cx, "before\n«selected\nˇ»after\n").await;
    context.update_editor(|editor, window, cx| {
        editor.add_inbox_thread_at_selection(window, cx);
    });
    context.run_until_parked();
    let (draft, content_id) = state.read_with(&context.cx, |state, cx| {
        let Some(draft) = state.drafts.first() else {
            panic!("Missing draft")
        };
        let Some((_, content_id, _)) = &draft.read(cx).draft else {
            panic!("Missing anchor")
        };
        (draft.clone(), content_id.clone())
    });
    draft.update_in(&mut context.cx, |draft, window, cx| {
        draft
            .input
            .update(cx, |input, cx| input.set_text("Unposted draft", window, cx));
    });
    let mut snapshot_row = row(file, content_id);
    publish(&mut context, &store, snapshot_row.clone());
    let thread = state.read_with(&context.cx, |state, _| {
        let Some(thread) = state.threads.get("test-thread") else {
            panic!("Missing thread")
        };
        thread.clone()
    });
    thread.update_in(&mut context.cx, |thread, window, cx| {
        thread.expanded = true;
        thread
            .input
            .update(cx, |input, cx| input.set_text("Unposted reply", window, cx));
    });
    snapshot_row
        .messages
        .push(message(2, Source::Agent, "Agent response"));
    publish(&mut context, &store, snapshot_row.clone());
    thread.read_with(&context.cx, |thread, cx| {
        assert_eq!(thread.input.read(cx).text(cx), "Unposted reply");
        assert!(thread.expanded);
        assert_eq!(thread.row.as_ref().map(|row| row.messages.len()), Some(2));
    });
    thread.update_in(&mut context.cx, |thread, window, cx| {
        thread.editing = Some((1, "Original question".into()));
        thread
            .input
            .update(cx, |input, cx| input.set_text("Unsaved edit", window, cx));
    });
    snapshot_row
        .messages
        .push(message(3, Source::Agent, "Another response"));
    publish(&mut context, &store, snapshot_row);
    thread.read_with(&context.cx, |thread, cx| {
        assert_eq!(thread.editing, Some((1, "Original question".into())));
        assert_eq!(thread.input.read(cx).text(cx), "Unsaved edit");
    });
    draft.read_with(&context.cx, |draft, cx| {
        assert_eq!(draft.input.read(cx).text(cx), "Unposted draft");
        assert!(draft.draft.is_some());
    });
    context.update_buffer(|buffer, cx| {
        buffer.edit(
            [(Point::new(0, 0)..Point::new(0, 0), "inserted\n")],
            None,
            cx,
        );
    });
    context.run_until_parked();
    let blocks = state.read_with(&context.cx, |state, _| {
        assert!(matches!(
            state.locations.get("test-thread"),
            Some(LocateResult::Matched {
                start: 2,
                count: 1,
                ..
            })
        ));
        assert_eq!(state.blocks.len(), 2);
        state.blocks.clone()
    });
    context.update_editor(|editor, _, cx| {
        assert!(
            blocks
                .iter()
                .any(|block| editor.row_for_block(*block, cx) == Some(DisplayRow(3)))
        );
    });
}

#[gpui::test]
async fn inbox_resolved_visibility_and_unmatched_file_level(cx: &mut TestAppContext) {
    let (mut context, store, state, file) = setup(cx, "ˇfirst\nsecond\n").await;
    let mut snapshot_row = row(file, "invalid-anchor".into());
    publish(&mut context, &store, snapshot_row.clone());
    let blocks = state.read_with(&context.cx, |state, cx| {
        assert_eq!(
            state.locations.get("test-thread"),
            Some(&LocateResult::Invalid)
        );
        assert_eq!(state.threads.len(), 1);
        for thread in state.threads.values() {
            assert!(thread.read(cx).location_label.contains("file-level"));
        }
        state.blocks.clone()
    });
    context.update_editor(|editor, _, cx| {
        for block in blocks {
            assert_eq!(editor.row_for_block(block, cx), Some(DisplayRow(1)));
        }
    });
    snapshot_row.thread.resolved = true;
    publish(&mut context, &store, snapshot_row.clone());
    state.read_with(&context.cx, |state, _| {
        assert!(state.threads.is_empty());
        assert!(state.blocks.is_empty());
    });
    store.update(&mut context.cx, |store, cx| store.toggle_resolved(cx));
    context.run_until_parked();
    state.read_with(&context.cx, |state, _| assert_eq!(state.threads.len(), 1));
    store.update(&mut context.cx, |store, cx| store.toggle_resolved(cx));
    context.run_until_parked();
    snapshot_row.thread.resolved = false;
    publish(&mut context, &store, snapshot_row);
    context.update_editor(|editor, window, cx| {
        editor.show_inbox_thread("test-thread", window, cx);
    });
    context.run_until_parked();
    state.read_with(&context.cx, |state, cx| {
        assert_eq!(state.threads.len(), 1);
        assert!(
            state
                .threads
                .values()
                .all(|thread| thread.read(cx).expanded)
        );
    });
}

fn create_draft(
    context: &mut EditorTestContext,
    state: &Entity<InboxEditor>,
) -> Entity<InboxThread> {
    context.update_editor(|editor, window, cx| {
        editor.add_inbox_thread_at_selection(window, cx);
    });
    context.run_until_parked();
    state.read_with(&context.cx, |state, _| {
        let Some(draft) = state.drafts.last() else {
            panic!("Expected a source-anchored draft");
        };
        draft.clone()
    })
}

#[gpui::test]
async fn inbox_keyboard_post_failure_preserves_intended_draft(cx: &mut TestAppContext) {
    let (mut context, _, state, _) = setup(cx, "«selected\nˇ»").await;
    let first = create_draft(&mut context, &state);
    first.read_with(&context.cx, |draft, _| {
        let Some((_, content_id, _)) = &draft.draft else {
            panic!("Expected a draft anchor");
        };
        assert!(matches!(
            anchors::locate_content_id("selected\n", content_id, &LocateOptions::default()),
            Ok(LocateResult::Matched {
                start: 0,
                count: 1,
                ..
            })
        ));
    });
    first.update_in(&mut context.cx, |thread, window, cx| {
        thread.input.update(cx, |input, cx| {
            input.set_text("First unposted draft", window, cx);
        });
    });
    let second = create_draft(&mut context, &state);
    second.update_in(&mut context.cx, |thread, window, cx| {
        thread.input.update(cx, |input, cx| {
            input.set_text("Second unposted draft", window, cx);
        });
        window.focus(&thread.input.focus_handle(cx), cx);
    });
    context.cx.update(|_, cx| reload_keymaps(cx));
    context.simulate_keystrokes("shift-enter");
    context.run_until_parked();
    second.read_with(&context.cx, |thread, cx| {
        assert!(!thread.busy);
        assert!(thread.error.is_none());
        assert!(thread.input.read(cx).text(cx).contains('\n'));
    });
    second.update_in(&mut context.cx, |thread, window, cx| {
        thread.input.update(cx, |input, cx| {
            input.set_text("Second unposted draft", window, cx);
        });
    });
    context.simulate_keystrokes("enter");
    context.run_until_parked();
    first.read_with(&context.cx, |thread, cx| {
        assert!(thread.draft.is_some());
        assert!(thread.error.is_none());
        assert_eq!(thread.input.read(cx).text(cx), "First unposted draft");
    });
    second.read_with(&context.cx, |thread, cx| {
        assert!(!thread.busy);
        assert!(thread.draft.is_some());
        assert_eq!(thread.input.read(cx).text(cx), "Second unposted draft");
        assert_eq!(
            thread.error.as_deref(),
            Some("Test store does not accept mutations")
        );
    });
    state.read_with(&context.cx, |state, _| {
        assert_eq!(state.drafts.len(), 2);
        assert_eq!(state.blocks.len(), 2);
    });
    context.simulate_keystrokes("escape");
    context.run_until_parked();
    state.read_with(&context.cx, |state, _| {
        assert_eq!(state.drafts.len(), 1);
        assert_eq!(state.blocks.len(), 1);
        assert_eq!(
            state.drafts.first().map(Entity::entity_id),
            Some(first.entity_id())
        );
    });
    context.update_editor(|editor, window, cx| {
        assert!(editor.focus_handle(cx).is_focused(window));
    });
}

#[gpui::test]
async fn inbox_keyboard_edit_failure_and_cancel_restore_focus(cx: &mut TestAppContext) {
    let (mut context, store, state, file) = setup(cx, "«selected\nˇ»").await;
    let draft = create_draft(&mut context, &state);
    let content_id = draft.read_with(&context.cx, |draft, _| {
        let Some((_, content_id, _)) = &draft.draft else {
            panic!("Expected a draft anchor");
        };
        content_id.clone()
    });
    draft.update_in(&mut context.cx, |draft, window, cx| {
        draft.discard(window, cx)
    });
    context.run_until_parked();
    publish(&mut context, &store, row(file, content_id));
    let thread = state.read_with(&context.cx, |state, _| {
        let Some(thread) = state.threads.get("test-thread") else {
            panic!("Expected a published thread");
        };
        thread.clone()
    });
    thread.update_in(&mut context.cx, |thread, window, cx| {
        thread.expanded = true;
        thread.editing = Some((1, "Original question".into()));
        thread.input.update(cx, |input, cx| {
            input.set_text("Unsaved keyboard edit", window, cx);
        });
    });
    state.update_in(&mut context.cx, |state, window, cx| {
        state.install_blocks(window, cx)
    });
    thread.update_in(&mut context.cx, |thread, window, cx| {
        window.focus(&thread.input.focus_handle(cx), cx);
    });
    context.cx.update(|_, cx| reload_keymaps(cx));
    context.simulate_keystrokes("enter");
    context.run_until_parked();
    thread.read_with(&context.cx, |thread, cx| {
        assert!(!thread.busy);
        assert_eq!(thread.editing, Some((1, "Original question".into())));
        assert_eq!(thread.input.read(cx).text(cx), "Unsaved keyboard edit");
        assert_eq!(
            thread.error.as_deref(),
            Some("Test store does not accept mutations")
        );
    });
    context.simulate_keystrokes("escape");
    context.run_until_parked();
    thread.read_with(&context.cx, |thread, cx| {
        assert!(thread.editing.is_none());
        assert!(thread.input.read(cx).text(cx).is_empty());
        assert!(thread.expanded);
    });
    context.simulate_keystrokes("escape");
    context.run_until_parked();
    thread.read_with(&context.cx, |thread, _| assert!(!thread.expanded));
    context.update_editor(|editor, window, cx| {
        assert!(editor.focus_handle(cx).is_focused(window));
    });
}
