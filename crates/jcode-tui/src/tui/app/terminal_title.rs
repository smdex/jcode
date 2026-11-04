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
        use crate::tui::TuiState;
        let mut metadata = MetadataReport::default();
        if let Some(id) = self.active_client_session_id() {
            let title = self.terminal_title.borrow();
            let base = if title.session_id == id && !title.base.is_empty() {
                Some(title.base.clone())
            } else {
                self.session_display_name()
            };
            let work = if self.is_processing() {
                self.display_turn_duration_secs()
                    .filter(|secs| secs.is_finite() && *secs >= 0.0)
                    .map(Duration::from_secs_f32)
            } else if title.session_id == id {
                title.last_work
            } else {
                None
            };
            metadata.title = base.filter(|base| !base.is_empty()).map(|base| {
                metadata_text(
                    &title_with_metrics(
                        &base,
                        self.display_edit_line_counts,
                        work,
                        self.is_processing(),
                    ),
                    80,
                )
            });
        }
        if !self.is_remote || !self.remote_session_facts_provisional {
            let (model, provider, thinking) = if self.is_remote {
                (
                    self.remote_provider_model.clone(),
                    self.remote_provider_name.clone(),
                    self.remote_reasoning_effort.clone(),
                )
            } else {
                (
                    Some(TuiState::provider_model(self)),
                    Some(TuiState::provider_name(self)),
                    self.provider.reasoning_effort(),
                )
            };
            for (key, value) in [
                ("model", model),
                ("provider", provider),
                ("thinking", thinking),
            ] {
                if let Some(value) = value {
                    let value = metadata_text(&value, 80);
                    if !value.is_empty() && value != "unknown" {
                        metadata.tokens.push((key, value));
                    }
                }
            }
            if self.context_limit > 0 {
                if let Some(used) = self.current_stream_context_tokens() {
                    let percent = used.saturating_mul(100) / self.context_limit;
                    metadata.tokens.push(("ctx", format!("{percent}%")));
                }
            }
        }
        // Local restores do not hydrate accounting; session totals need authoritative history.
        if let Some((history_in, history_out)) = self.remote_total_tokens.filter(|_| self.is_remote)
        {
            metadata.tokens.extend([
                (
                    "input-tokens",
                    history_in
                        .saturating_add(self.token_accounting.total_input_tokens)
                        .to_string(),
                ),
                (
                    "output-tokens",
                    history_out
                        .saturating_add(self.token_accounting.total_output_tokens)
                        .to_string(),
                ),
            ]);
        }
        if active.is_empty() {
            return (state, metadata);
        }
        metadata
            .tokens
            .push(("subagent-count", active.len().to_string()));
        metadata.tokens.push((
            "subagent-attention",
            active
                .iter()
                .filter(|m| matches!(m.status.as_str(), "blocked" | "waiting_network"))
                .count()
                .to_string(),
        ));
        // ponytail: show three live descendants, add paginated metadata if larger swarms need full detail.
        for (member, (info_key, task_key)) in active.iter().take(3).zip([
            ("subagent-1", "subagent-task-1"),
            ("subagent-2", "subagent-task-2"),
            ("subagent-3", "subagent-task-3"),
        ]) {
            let mut info = vec![
                metadata_text(
                    member
                        .friendly_name
                        .as_deref()
                        .unwrap_or(&member.session_id),
                    24,
                ),
                metadata_text(&member.status, 24),
            ];
            for value in [&member.runtime.model, &member.runtime.effort] {
                if let Some(value) = value {
                    info.push(metadata_text(value, 32));
                }
            }
            if let (Some(used), Some(limit)) =
                (member.runtime.context_tokens, member.runtime.context_limit)
            {
                if limit > 0 {
                    info.push(format!("ctx {}%", used.saturating_mul(100) / limit));
                }
            }
            if let Some((done, total)) = member.todo_progress {
                info.push(format!("todos {done}/{total}"));
            }
            metadata
                .tokens
                .push((info_key, metadata_text(&info.join(" · "), 80)));
            if let Some(task) = &member.task_label {
                metadata.tokens.push((task_key, metadata_text(task, 80)));
            }
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
        metadata.summary = Some(metadata_text(&summary, 80));
        metadata.title_suffix = Some(format!("⏳{count}{}", if attention { "⚠" } else { "" }));
        (state, metadata)
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
        use crate::tui::herdr::AgentState::{Idle, Working};
        let mut app = super::super::tests::create_test_app();
        app.is_remote = true;
        app.remote_session_id = Some("parent".into());
        app.swarm_enabled = true;
        let member = |id: &str, parent: &str, status: &str| {
            serde_json::from_value(serde_json::json!({
                "session_id": id, "friendly_name": id, "status": status,
                "task_label": "Review\n東京\u{1b} task", "report_back_to_session_id": parent,
                "runtime": {"model": "child-model", "effort": "high", "context_tokens": 50, "context_limit": 100},
                "todo_progress": [1, 2],
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
        assert!(
            metadata
                .tokens
                .iter()
                .any(|(key, value)| *key == "subagent-count" && value == "2")
        );
        assert!(
            metadata
                .tokens
                .iter()
                .any(|(key, value)| *key == "subagent-attention" && value == "1")
        );
        let child = &metadata
            .tokens
            .iter()
            .find(|(key, _)| *key == "subagent-1")
            .unwrap()
            .1;
        assert!(
            child.contains("scout") && child.contains("running") && child.contains("child-model")
        );
        assert!(child.contains("high") && child.contains("50%") && child.contains("1/2"));
        for (_, value) in &metadata.tokens {
            assert!(!value.chars().any(char::is_control));
            assert!(unicode_width::UnicodeWidthStr::width(value.as_str()) <= 80);
        }
        app.remote_session_id = Some("unrelated".into());
        let (state, metadata) = app.herdr_state_and_metadata();
        assert_eq!(state, Idle);
        assert!(metadata.summary.is_none() && metadata.title_suffix.is_none());
        assert!(
            !metadata
                .tokens
                .iter()
                .any(|(key, _)| key.starts_with("subagent"))
        );
        app.remote_session_id = Some("parent".into());
        for status in ["ready", "completed", "failed", "crashed", "stopped", "done"] {
            app.handle_server_event(
                ServerEvent::SwarmStatus {
                    members: vec![member("scout", "parent", status)],
                },
                &mut remote,
            );
            let (state, metadata) = app.herdr_state_and_metadata();
            assert_eq!(state, Idle, "{status}");
            assert!(metadata.summary.is_none() && metadata.title_suffix.is_none());
            assert!(
                !metadata
                    .tokens
                    .iter()
                    .any(|(key, _)| key.starts_with("subagent"))
            );
        }
    }

    #[test]
    fn herdr_disconnect_invalidates_children_until_authoritative_status_returns() {
        use crate::protocol::ServerEvent;
        use crate::tui::herdr::AgentState::{Idle, Working};
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
        let (state, metadata) = app.herdr_state_and_metadata();
        assert_eq!(state, Idle);
        assert!(metadata.summary.is_none() && metadata.title_suffix.is_none());
        assert!(
            !metadata
                .tokens
                .iter()
                .any(|(key, _)| key.starts_with("subagent"))
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
    fn herdr_parent_metadata_uses_confirmed_facts_and_clears_stale_context() {
        let mut app = super::super::tests::create_test_app();
        app.is_remote = true;
        app.remote_session_id = Some("parent".into());
        app.suppress_terminal_title_updates = true;
        app.set_terminal_title_base("parent", "Review 東京".into());
        app.remote_session_facts_provisional = false;
        app.remote_provider_model = Some("test-model".into());
        app.remote_provider_name = Some("test-provider".into());
        app.remote_reasoning_effort = Some("high".into());
        app.context_limit = 1000;
        app.streaming.streaming_input_tokens = 250;
        app.remote_total_tokens = Some((10, 20));
        app.token_accounting.total_input_tokens = 30;
        app.token_accounting.total_output_tokens = 40;
        app.display_edit_line_counts = (3, 2);
        app.terminal_title.borrow_mut().last_work = Some(Duration::from_secs(125));
        let (_, metadata) = app.herdr_state_and_metadata();
        for (key, expected) in [
            ("model", "test-model"),
            ("provider", "test-provider"),
            ("thinking", "high"),
            ("ctx", "25%"),
            ("input-tokens", "40"),
            ("output-tokens", "60"),
        ] {
            assert!(
                metadata
                    .tokens
                    .iter()
                    .any(|(name, value)| *name == key && value == expected),
                "{key}: {:?}",
                metadata.tokens
            );
        }
        assert_eq!(
            metadata.title.as_deref(),
            Some("Review 東京 · +3 -2 · last ~2m05s")
        );
        assert!(
            metadata.summary.is_none(),
            "parent facts must not replace lifecycle labels"
        );
        app.streaming.streaming_context_stale = true;
        let (_, metadata) = app.herdr_state_and_metadata();
        assert!(!metadata.tokens.iter().any(|(key, _)| *key == "ctx"));
        app.remote_session_facts_provisional = true;
        let (_, metadata) = app.herdr_state_and_metadata();
        assert!(
            !metadata
                .tokens
                .iter()
                .any(|(key, _)| matches!(*key, "model" | "provider" | "thinking" | "ctx"))
        );
        app.remote_session_id = Some("other".into());
        let (_, metadata) = app.herdr_state_and_metadata();
        assert!(
            metadata
                .title
                .is_none_or(|title| !title.contains("Review") && !title.contains("last"))
        );
    }

    #[test]
    fn herdr_omits_unknown_restored_token_totals() {
        let mut app = super::super::tests::create_test_app();
        app.token_accounting.total_input_tokens = 30;
        app.token_accounting.total_output_tokens = 40;
        for remote in [false, true] {
            app.is_remote = remote;
            app.remote_total_tokens = None;
            let (_, metadata) = app.herdr_state_and_metadata();
            assert!(
                !metadata
                    .tokens
                    .iter()
                    .any(|(key, _)| matches!(*key, "input-tokens" | "output-tokens"))
            );
        }
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
