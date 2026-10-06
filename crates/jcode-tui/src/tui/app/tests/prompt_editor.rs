#[test]
fn external_prompt_editor_local_key_preserves_draft_and_bookmarks() {
    let mut app = create_test_app();
    app.input = "original draft".into();
    app.cursor_pos = 3;
    app.handle_key(KeyCode::Char('g'), KeyModifiers::CONTROL)
        .unwrap();
    assert!(app.pending_prompt_editor);
    assert_eq!(app.input, "original draft");
    assert_eq!(app.cursor_pos, 3);
    assert!(!app.pending_turn);
    app.pending_prompt_editor = false;
    app.scroll_offset = 5;
    app.auto_scroll_paused = true;
    app.handle_key(KeyCode::Char('g'), KeyModifiers::CONTROL)
        .unwrap();
    assert!(!app.pending_prompt_editor);
    assert_eq!(app.scroll_bookmark, Some(5));
    assert_eq!(app.scroll_offset, 0);
    app.handle_key(KeyCode::Char('g'), KeyModifiers::CONTROL)
        .unwrap();
    assert!(!app.pending_prompt_editor);
    assert_eq!(app.scroll_offset, 5);
}

#[test]
fn external_prompt_editor_remote_key_requests_same_action() {
    let mut app = create_test_app();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        app.input = "remote draft".into();
        app.handle_remote_key(KeyCode::Char('g'), KeyModifiers::CONTROL, &mut remote)
            .await
            .unwrap();
        assert!(app.pending_prompt_editor);
        assert_eq!(app.input, "remote draft");
        assert!(!app.pending_turn);
    });
}

#[test]
fn external_prompt_editor_apply_is_undoable_and_unchanged_keeps_cursor() {
    let mut app = create_test_app();
    app.input = "raw draft".into();
    app.cursor_pos = 2;
    app.pasted_contents.push("retained paste".into());
    app.pending_images
        .push(("image/png".into(), "aGVsbG8=".into()));
    app.apply_prompt_editor_result("expanded draft", "expanded draft".into());
    assert_eq!(app.input, "raw draft");
    assert_eq!(app.cursor_pos, 2);
    app.apply_prompt_editor_result("expanded draft", "edited\n世界".into());
    assert_eq!(app.input, "edited\n世界");
    assert_eq!(app.cursor_pos, app.input.len());
    assert_eq!(app.pasted_contents, vec!["retained paste"]);
    assert_eq!(app.pending_images.len(), 1);
    app.handle_key(KeyCode::Char('z'), KeyModifiers::CONTROL)
        .unwrap();
    assert_eq!(app.input, "raw draft");
    assert_eq!(app.cursor_pos, 2);
    assert!(!app.pending_turn);
    app.apply_prompt_editor_result("raw draft", String::new());
    assert!(app.input.is_empty());
    assert_eq!(app.cursor_pos, 0);
}

#[test]
fn external_prompt_editor_rejects_other_modifiers_and_focused_panes() {
    let mut app = create_test_app();
    assert!(!app.request_prompt_editor_for_key(KeyCode::Char('g'), KeyModifiers::ALT));
    assert!(!app.request_prompt_editor_for_key(
        KeyCode::Char('g'),
        KeyModifiers::CONTROL | KeyModifiers::ALT
    ));
    app.diff_pane_focus = true;
    assert!(!app.request_prompt_editor_for_key(KeyCode::Char('g'), KeyModifiers::CONTROL));
    app.diff_pane_focus = false;
    app.diagram_focus = true;
    assert!(!app.request_prompt_editor_for_key(KeyCode::Char('g'), KeyModifiers::CONTROL));
}
