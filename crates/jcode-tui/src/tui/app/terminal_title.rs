//! Cheap, session-scoped metadata for the terminal's native tabs.
//!
//! Diff counts are cumulative edit-output lines, matching the transcript's diff
//! badges, not a repository-wide `git diff` (other sessions may share the repo).
//! Time is the current/last observed working episode (including continuations),
//! never the age of an open tab or a fabricated lifetime total.
use super::{App, DisplayMessage};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct TerminalTitleState {
    pub base: String,
    pub session_id: String,
    pub last_sent: String,
    pub last_refresh: Option<Instant>,
    pub last_work: Option<Duration>,
}

pub(super) fn edit_line_counts(message: &DisplayMessage) -> (usize, usize) {
    let Some(tool) = message.tool_data.as_ref().filter(|tool| {
        message.role == "tool"
            && crate::tui::ui::tools_ui::is_edit_tool_name(&tool.name)
            && jcode_tui_tool_display::concise_tool_error_summary(&message.content).is_none()
    }) else {
        return (0, 0);
    };
    crate::tui::ui_diff::diff_change_counts_for_tool(tool, &message.content)
}

fn work_label(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, secs / 60 % 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn title_with_metrics(
    base: &str,
    counts: (usize, usize),
    work: Option<Duration>,
    active: bool,
) -> String {
    let mut title = base.to_owned();
    if counts != (0, 0) {
        title.push_str(&format!(" · +{} -{}", counts.0, counts.1));
    }
    if let Some(work) = work {
        title.push_str(&format!(
            " · {} ~{}",
            if active { "work" } else { "last" },
            work_label(work)
        ));
    } else if active {
        title.push_str(" · working");
    }
    title
}

fn metadata_text(text: &str, width: usize) -> String {
    let clean: String = text
        .chars()
        .take(80)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    jcode_tui_render::truncate_line_with_ellipsis_to_width(&ratatui::text::Line::from(clean), width)
        .to_string()
}

impl App {
    /// Tell herdr (when running in a herdr pane) whether this client is
    /// working or idle and which session to resume. Tick-safe: unchanged
    /// state is a cheap comparison and reports are sent off-thread.
    pub(super) fn sync_herdr_agent_state(&self) {
        if self.suppress_terminal_title_updates
            || self.is_replay_runtime()
            || !crate::tui::herdr::enabled()
        {
            return;
        }
        let (state, metadata) = self.herdr_state_and_metadata();
        crate::tui::herdr::sync(state, self.active_client_session_id(), metadata);
    }

    fn herdr_state_and_metadata(
        &self,
    ) -> (
        crate::tui::herdr::AgentState,
        crate::tui::herdr::MetadataReport,
    ) {
        use crate::tui::herdr::{AgentState, MetadataReport};
        let members = self
            .active_client_session_id()
            .map(|id| super::tui_state::filter_inline_swarm_subtree(&self.remote_swarm_members, id))
            .unwrap_or_default();
        let active: Vec<_> = members
            .iter()
            .filter(|member| {
                jcode_tui_render::swarm_gallery::is_active_status(&member.status)
                    || matches!(member.status.as_str(), "blocked" | "waiting_network")
            })
            .collect();
        // Child attention belongs to the parent agent, not a human-blocked pane.
        let attention = active
            .iter()
            .any(|member| matches!(member.status.as_str(), "blocked" | "waiting_network"));
        let state = if self.is_processing() || !active.is_empty() {
            AgentState::Working
        } else {
            AgentState::Idle
        };
        if active.is_empty() {
            return (state, MetadataReport::default());
        }
        let names = active
            .iter()
            .take(3)
            .map(|member| {
                metadata_text(
                    member
                        .friendly_name
                        .as_deref()
                        .unwrap_or(&member.session_id),
                    24,
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let task = active
            .iter()
            .find_map(|member| member.task_label.as_deref())
            .map(|task| metadata_text(task, 60))
            .filter(|task| !task.is_empty());
        let count = active.len();
        let summary = format!(
            "⏳ {count} subagent{}{} ({names}{}){}",
            if count == 1 { "" } else { "s" },
            if attention { " ⚠" } else { "" },
            if count > 3 { ", …" } else { "" },
            task.map(|task| format!(" · {task}")).unwrap_or_default(),
        );
        (
            state,
            MetadataReport {
                summary: Some(metadata_text(&summary, 80)),
                title_suffix: Some(format!("⏳{count}{}", if attention { "⚠" } else { "" })),
            },
        )
    }

    pub(super) fn set_terminal_title_base(&self, session_id: &str, base: String) {
        let mut state = self.terminal_title.borrow_mut();
        if state.session_id != session_id {
            *state = TerminalTitleState::default();
            state.session_id = session_id.to_owned();
        }
        state.base = base;
        state.last_refresh = None;
        drop(state);
        self.refresh_terminal_title_metrics();
    }

    /// Tick-safe: no filesystem access, transcript scans, or subprocesses.
    /// At most one OSC title update per second, and only if the text changed.
    pub(super) fn refresh_terminal_title_metrics(&self) {
        if self.suppress_terminal_title_updates {
            return;
        }
        let mut state = self.terminal_title.borrow_mut();
        if state.base.is_empty()
            || state
                .last_refresh
                .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        state.last_refresh = Some(Instant::now());
        let active = self.is_processing();
        let work = if active {
            self.display_turn_duration_secs()
                .filter(|secs| secs.is_finite() && *secs >= 0.0)
                .map(Duration::from_secs_f32)
        } else {
            state.last_work
        };
        if active && work.is_some() {
            state.last_work = work;
        }
        let title = title_with_metrics(&state.base, self.display_edit_line_counts, work, active);
        if state.last_sent != title
            && crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(&title)).is_ok()
        {
            state.last_sent = title;
        }
    }

    pub(super) fn remember_terminal_title_work(&self) {
        if let Some(secs) = self
            .display_turn_duration_secs()
            .filter(|secs| secs.is_finite() && *secs >= 0.0)
        {
            let mut state = self.terminal_title.borrow_mut();
            state.last_work = Some(Duration::from_secs_f32(secs));
            state.last_refresh = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    #[test]
    fn herdr_state_follows_send_done_interrupt_error_and_queued_work() {
        use crate::protocol::ServerEvent;
        use crate::tui::herdr::AgentState::{Idle, Working};
        let mut app = super::super::tests::create_test_app();
        app.is_remote = true;
        app.remote_session_id = Some("parent".into());
        app.auto_poke_incomplete_todos = false;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut remote = crate::tui::backend::RemoteConnection::dummy();
            assert_eq!(app.herdr_state_and_metadata().0, Idle);
            for ending in ["done", "interrupted", "error"] {
                let id = super::super::remote::begin_remote_send(
                    &mut app,
                    &mut remote,
                    "hello".into(),
                    vec![],
                    false,
                    None,
                    false,
                    0,
                )
                .await
                .unwrap();
                assert_eq!(app.herdr_state_and_metadata().0, Working);
                app.handle_server_event(ServerEvent::Done { id: id + 100 }, &mut remote);
                assert_eq!(app.herdr_state_and_metadata().0, Working, "unrelated Done");
                let event = match ending {
                    "done" => ServerEvent::Done { id },
                    "interrupted" => ServerEvent::Interrupted,
                    _ => ServerEvent::Error {
                        id,
                        message: "test failure".into(),
                        retry_after_secs: None,
                    },
                };
                app.handle_server_event(event, &mut remote);
                assert_eq!(app.herdr_state_and_metadata().0, Idle, "{ending}");
            }
            app.pending_queued_dispatch = true;
            assert_eq!(app.herdr_state_and_metadata().0, Working);
            app.pending_queued_dispatch = false;
            assert_eq!(app.herdr_state_and_metadata().0, Idle);
        });
    }

    #[test]
    fn herdr_metadata_tracks_only_current_sessions_live_descendants() {
        use crate::protocol::ServerEvent;
        use crate::tui::herdr::{
            AgentState::{Idle, Working},
            MetadataReport,
        };
        let mut app = super::super::tests::create_test_app();
        app.is_remote = true;
        app.remote_session_id = Some("parent".into());
        app.swarm_enabled = true;
        let member = |id: &str, parent: &str, status: &str| {
            serde_json::from_value(serde_json::json!({
                "session_id": id, "friendly_name": id, "status": status,
                "task_label": "Review\n東京\u{1b} task", "report_back_to_session_id": parent,
            }))
            .unwrap()
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        app.handle_server_event(
            ServerEvent::SwarmStatus {
                members: vec![
                    member("parent", "someone", "running"),
                    member("scout", "parent", "running"),
                    member("reviewer", "scout", "blocked"),
                    member("peer", "someone", "running"),
                    member("finished", "parent", "completed"),
                ],
            },
            &mut remote,
        );
        let (state, metadata) = app.herdr_state_and_metadata();
        assert_eq!(state, Working, "live children keep an idle parent busy");
        let summary = metadata.summary.unwrap();
        assert!(summary.contains("2 subagents"), "{summary}");
        assert!(summary.contains("scout") && summary.contains("reviewer"));
        assert!(summary.contains('⚠'));
        assert!(!summary.contains("peer") && !summary.contains("finished"));
        assert!(!summary.chars().any(char::is_control));
        assert!(metadata.title_suffix.unwrap().contains("2"));
        app.remote_session_id = Some("unrelated".into());
        assert_eq!(
            app.herdr_state_and_metadata(),
            (Idle, MetadataReport::default())
        );
        app.remote_session_id = Some("parent".into());
        for status in ["ready", "completed", "failed", "crashed", "stopped", "done"] {
            app.handle_server_event(
                ServerEvent::SwarmStatus {
                    members: vec![member("scout", "parent", status)],
                },
                &mut remote,
            );
            assert_eq!(
                app.herdr_state_and_metadata(),
                (Idle, MetadataReport::default()),
                "{status}"
            );
        }
    }

    #[test]
    fn herdr_disconnect_invalidates_children_until_authoritative_status_returns() {
        use crate::protocol::ServerEvent;
        use crate::tui::herdr::{
            AgentState::{Idle, Working},
            MetadataReport,
        };
        let mut app = super::super::tests::create_test_app();
        app.is_remote = true;
        app.remote_session_id = Some("parent".into());
        app.swarm_enabled = true;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        let member = serde_json::from_value(serde_json::json!({
            "session_id": "child", "status": "running",
            "report_back_to_session_id": "parent",
        }))
        .unwrap();
        app.handle_server_event(
            ServerEvent::SwarmStatus {
                members: vec![member],
            },
            &mut remote,
        );
        assert_eq!(app.herdr_state_and_metadata().0, Working);
        // A redraw immediately after disconnect must not re-report cosmetic
        // split-launch activity or queued-but-undispatched work as Working.
        app.pending_split_started_at = Some(Instant::now());
        app.pending_queued_dispatch = true;
        app.queued_messages.push("keep this follow-up".into());
        super::super::remote::handle_disconnect(
            &mut app,
            &mut super::super::remote::RemoteRunState::default(),
            None,
        );
        assert_eq!(
            app.herdr_state_and_metadata(),
            (Idle, MetadataReport::default())
        );
        assert!(app.remote_swarm_members_received_at.is_none());
        assert!(!app.is_processing());
        assert_eq!(app.queued_messages, ["keep this follow-up"]);
        app.sync_herdr_agent_state();
        let member = serde_json::from_value(serde_json::json!({
            "session_id": "child", "status": "running",
            "report_back_to_session_id": "parent",
        }))
        .unwrap();
        app.handle_server_event(
            ServerEvent::SwarmStatus {
                members: vec![member],
            },
            &mut remote,
        );
        assert_eq!(app.herdr_state_and_metadata().0, Working);
    }

    #[test]
    fn herdr_metadata_is_bounded_in_characters_and_display_cells() {
        for text in [
            "東京".repeat(90),
            "e\u{301}".repeat(90),
            "\n\t\u{1b}".repeat(90),
        ] {
            let text = metadata_text(&text, 80);
            assert!(text.chars().count() <= 80);
            assert!(unicode_width::UnicodeWidthStr::width(text.as_str()) <= 80);
            assert!(!text.chars().any(char::is_control));
        }
    }

    #[test]
    fn terminal_title_counts_follow_append_remove_and_history_replace() {
        let mut app = super::super::tests::create_test_app();
        app.suppress_terminal_title_updates = true;
        let message = DisplayMessage::tool(
            "@@ -1 +1 @@\n-old\n+new\n+extra",
            ToolCall {
                id: "edit-1".into(),
                name: "edit".into(),
                input: serde_json::json!({"file_path": "example.rs"}),
                intent: None,
                thought_signature: None,
            },
        );
        app.push_display_message(message.clone());
        assert_eq!(app.display_edit_line_counts, (2, 1));
        app.push_display_message(DisplayMessage::user("continue"));
        assert_eq!(app.display_edit_line_counts, (2, 1));
        app.adjust_display_message_stats(&message, false);
        assert_eq!(app.display_edit_line_counts, (0, 0));
        app.replace_display_messages(vec![message.clone(), message]);
        assert_eq!(app.display_edit_line_counts, (4, 2));
        app.replace_display_messages(vec![]);
        assert_eq!(app.display_edit_line_counts, (0, 0));
    }

    #[test]
    fn terminal_title_work_freezes_and_resets_on_session_switch() {
        let mut app = super::super::tests::create_test_app();
        app.suppress_terminal_title_updates = true;
        app.set_terminal_title_base("first", "First task".into());
        app.visible_turn_started = Some(Instant::now() - Duration::from_secs(125));
        app.clear_visible_turn_started();
        let completed = app.terminal_title.borrow().last_work.unwrap();
        assert_eq!(completed.as_secs(), 125);
        app.remember_terminal_title_work();
        assert_eq!(app.terminal_title.borrow().last_work, Some(completed));
        app.set_terminal_title_base("first", "Renamed task".into());
        assert_eq!(app.terminal_title.borrow().last_work, Some(completed));
        app.set_terminal_title_base("second", "Second task".into());
        assert_eq!(app.terminal_title.borrow().last_work, None);
        assert!(app.terminal_title.borrow().last_sent.is_empty());
    }

    #[test]
    fn title_metrics_show_edits_and_current_or_last_turn() {
        let base = "🦊 Improve session tabs";
        assert_eq!(title_with_metrics(base, (0, 0), None, false), base);
        assert_eq!(
            title_with_metrics(base, (12, 3), Some(Duration::from_secs(125)), true),
            "🦊 Improve session tabs · +12 -3 · work ~2m05s"
        );
        assert_eq!(
            title_with_metrics(base, (0, 4), Some(Duration::from_secs(125)), false),
            "🦊 Improve session tabs · +0 -4 · last ~2m05s"
        );
        assert_eq!(
            title_with_metrics(base, (0, 0), None, true),
            "🦊 Improve session tabs · working"
        );
    }

    #[test]
    fn work_duration_boundaries_are_compact() {
        for (secs, expected) in [
            (0, "0s"),
            (59, "59s"),
            (60, "1m00s"),
            (3599, "59m59s"),
            (3600, "1h00m"),
            (90061, "25h01m"),
        ] {
            assert_eq!(work_label(Duration::from_secs(secs)), expected);
        }
    }

    #[test]
    fn edit_counts_ignore_failures_and_non_edit_output() {
        let mut message = DisplayMessage::tool(
            "@@ -1 +1 @@\n-old\n+new\n+extra",
            ToolCall {
                id: "edit-1".into(),
                name: "edit".into(),
                input: serde_json::json!({"file_path": "example.rs"}),
                intent: None,
                thought_signature: None,
            },
        );
        assert_eq!(edit_line_counts(&message), (2, 1));
        message.tool_data.as_mut().unwrap().name = "bash".into();
        assert_eq!(edit_line_counts(&message), (0, 0));
        message.tool_data.as_mut().unwrap().name = "edit".into();
        message.content = "Error: could not apply patch\n-old\n+new".into();
        assert_eq!(edit_line_counts(&message), (0, 0));
    }
}
