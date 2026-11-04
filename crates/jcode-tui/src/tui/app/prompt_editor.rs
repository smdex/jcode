//! External editing of the unsent composer draft.
//!
//! Key handlers only request this action. The loop owner performs the terminal
//! handoff outside input burst drains, then invalidates the whole screen.

use super::{App, DisplayMessage};
use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyModifiers};
use std::io::{IsTerminal, Read};
use std::path::Path;
use std::process::{Command, ExitStatus};

const MAX_PROMPT_BYTES: u64 = 16 * 1024 * 1024;

fn editor_command(visual: Option<String>, editor: Option<String>) -> String {
    visual
        .filter(|value| !value.trim().is_empty())
        .or_else(|| editor.filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.to_string())
}

fn edit_prompt_with(
    editor: &str,
    draft: &str,
    scratch: &Path,
    working_dir: Option<&Path>,
    run: impl FnOnce(&mut Command) -> std::io::Result<ExitStatus>,
) -> Result<String> {
    let args = shlex::split(editor).context("Editor command contains unmatched quotes")?;
    let Some(program) = args.first().filter(|program| !program.is_empty()) else {
        bail!("Editor command is empty");
    };
    std::fs::create_dir_all(scratch).context("Could not create prompt scratch directory")?;
    // The private directory also keeps editor-created swap/backup files private
    // and removes them on success, cancellation, and errors. Editors may replace
    // the file atomically, so always reopen its path instead of reading an old fd.
    let mut directory_builder = tempfile::Builder::new();
    directory_builder.prefix("prompt-editor-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory_builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = directory_builder
        .tempdir_in(scratch)
        .context("Could not create private prompt file")?;
    let path = directory.path().join("prompt.md");
    std::fs::write(&path, draft).context("Could not write prompt file")?;
    let mut command = Command::new(program);
    command.args(&args[1..]).arg(&path);
    if let Some(dir) = working_dir.filter(|dir| dir.is_dir()) {
        command.current_dir(dir);
    }
    let status =
        run(&mut command).with_context(|| format!("Could not launch editor {editor:?}"))?;
    if !status.success() {
        bail!("Editor exited with {status}. Original draft kept");
    }
    let mut edited = String::new();
    std::fs::File::open(&path)
        .context("Editor removed the prompt file. Original draft kept")?
        .take(MAX_PROMPT_BYTES + 1)
        .read_to_string(&mut edited)
        .context("Could not read edited prompt as UTF-8. Original draft kept")?;
    if edited.len() as u64 > MAX_PROMPT_BYTES {
        bail!("Edited prompt exceeds 16 MiB. Original draft kept");
    }
    Ok(edited)
}

impl App {
    pub(super) fn request_prompt_editor_for_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        // Ctrl+G already navigates bookmarks in scrollback. At the bottom with
        // no saved bookmark it previously did nothing, so this does not displace
        // that action or any pane/picker binding.
        if code != KeyCode::Char('g')
            || modifiers != KeyModifiers::CONTROL
            || self.scroll_offset != 0
            || self.scroll_bookmark.is_some()
            || self.diff_pane_focus
            || self.diagram_focus
        {
            return false;
        }
        self.pending_prompt_editor = true;
        true
    }

    pub(super) fn apply_prompt_editor_result(&mut self, original_expanded: &str, edited: String) {
        if edited == original_expanded {
            self.set_status_notice("Prompt unchanged");
            return;
        }
        self.remember_input_undo_state();
        self.input = edited;
        self.cursor_pos = self.input.len();
        self.history_draft = None;
        self.reset_tab_completion();
        // Keep pending_images and pasted_contents: attachments remain attached,
        // and Ctrl+Z must still be able to restore collapsed paste placeholders.
        self.sync_model_picker_preview_from_input();
        self.set_status_notice("Prompt edited. Press Enter to send");
    }

    pub(super) fn service_prompt_editor(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
    ) -> bool {
        if !std::mem::take(&mut self.pending_prompt_editor) {
            return false;
        }
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            self.push_display_message(DisplayMessage::error(
                "Prompt editing needs an interactive terminal".to_string(),
            ));
            return true;
        }
        let expanded = super::input::expand_paste_placeholders(self, &self.input.clone());
        let editor = editor_command(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok());
        let result = crate::storage::jcode_dir()
            .map(|dir| dir.join("scratch"))
            .and_then(|scratch| {
                edit_prompt_with(
                    &editor,
                    &expanded,
                    &scratch,
                    self.session.working_dir.as_deref().map(Path::new),
                    |command| {
                        crate::tui::terminal_events::prepare_editor_handoff();
                        super::commands::run_interactive_editor(command)
                    },
                )
            });
        match result {
            Ok(edited) => self.apply_prompt_editor_result(&expanded, edited),
            Err(error) => self.push_display_message(DisplayMessage::error(format!(
                "Could not edit prompt: {error:#}. Draft unchanged"
            ))),
        }
        // Returning from an editor re-enters a blank alternate screen. Neither
        // ratatui's previous buffer nor animation-only caches may be reused.
        super::run_shell::invalidate_previous_terminal_buffer(terminal);
        self.request_full_redraw();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_editor_environment_precedence_and_blank_fallbacks() {
        assert_eq!(
            editor_command(Some("nvim -f".into()), Some("nano".into())),
            "nvim -f"
        );
        assert_eq!(
            editor_command(Some("  ".into()), Some("nano".into())),
            "nano"
        );
        assert_eq!(
            editor_command(None, Some(" ".into())),
            if cfg!(windows) { "notepad" } else { "vi" }
        );
    }

    #[cfg(unix)]
    #[test]
    fn prompt_editor_preserves_multiline_unicode_and_atomic_saves() {
        let root = tempfile::tempdir().unwrap();
        let original = "first line\nПривіт 世界\n";
        let edited = edit_prompt_with("sh -c 'exit 0'", original, root.path(), None, |command| {
            let path = std::path::PathBuf::from(command.get_args().last().unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            let replacement = path.with_extension("new");
            std::fs::write(&replacement, "changed\n世界\n").unwrap();
            std::fs::rename(replacement, path).unwrap();
            command.status()
        })
        .unwrap();
        assert_eq!(edited, "changed\n世界\n");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn prompt_editor_quoted_executable_and_arguments_are_not_shell_code() {
        let root = tempfile::tempdir().unwrap();
        let edited = edit_prompt_with(
            "'sh' -c 'exit 0' 'argument with spaces; literal'",
            "draft",
            root.path(),
            None,
            |command| {
                let args: Vec<_> = command
                    .get_args()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect();
                assert_eq!(
                    &args[..3],
                    &["-c", "exit 0", "argument with spaces; literal"]
                );
                command.status()
            },
        )
        .unwrap();
        assert_eq!(edited, "draft");
    }

    #[cfg(unix)]
    #[test]
    fn prompt_editor_failures_keep_draft_and_clean_private_files() {
        let root = tempfile::tempdir().unwrap();
        for editor in ["sh -c 'exit 7'", "jcode-nonexistent-prompt-editor"] {
            assert!(
                edit_prompt_with(editor, "original", root.path(), None, |command| {
                    std::fs::write(command.get_args().last().unwrap(), "saved but cancelled")
                        .unwrap();
                    command.status()
                })
                .is_err()
            );
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
        assert!(
            edit_prompt_with("'unclosed", "original", root.path(), None, Command::status).is_err()
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn prompt_editor_empty_edit_and_deleted_file_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        let empty = edit_prompt_with("sh -c 'exit 0'", "original", root.path(), None, |command| {
            std::fs::write(command.get_args().last().unwrap(), "").unwrap();
            command.status()
        })
        .unwrap();
        assert!(empty.is_empty());
        assert!(
            edit_prompt_with("sh -c 'exit 0'", "original", root.path(), None, |command| {
                std::fs::remove_file(command.get_args().last().unwrap()).unwrap();
                command.status()
            })
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn prompt_editor_rejects_invalid_utf8_and_oversized_files() {
        let root = tempfile::tempdir().unwrap();
        for oversized in [false, true] {
            let result =
                edit_prompt_with("sh -c 'exit 0'", "original", root.path(), None, |command| {
                    let path = command.get_args().last().unwrap();
                    if oversized {
                        std::fs::File::create(path)
                            .unwrap()
                            .set_len(MAX_PROMPT_BYTES + 1)
                            .unwrap();
                    } else {
                        std::fs::write(path, [0xff]).unwrap();
                    }
                    command.status()
                });
            assert!(result.is_err());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }
}
