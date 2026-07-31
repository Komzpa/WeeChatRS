use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use egui::text_edit::TextEditState;
use egui::text::{CCursorRange, CCursor};
use crate::relay::models::MentionCandidate;
use crate::ui::app::{
    CommandCompletionRequest, CompletionState, MentionCompletionState, SelectedMention,
    WeeChatApp,
};
use crate::ui::emoji;

fn selected_char_range(ctx: &egui::Context, id: egui::Id, text: &str) -> (usize, usize) {
    let end = text.chars().count();
    let Some(state) = TextEditState::load(ctx, id) else {
        return (end, end);
    };
    let Some(range) = state.cursor.char_range() else {
        return (end, end);
    };
    let [start, finish] = range.sorted();
    (start.index.min(end), finish.index.min(end))
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte_index, _)| byte_index)
}

fn apply_native_completion(
    input: &str,
    cursor_byte_idx: usize,
    position_replace: usize,
    base_word: &str,
    matched: &str,
    add_space: bool,
) -> Option<(String, usize)> {
    if position_replace > cursor_byte_idx
        || cursor_byte_idx > input.len()
        || !input.is_char_boundary(position_replace)
        || !input.is_char_boundary(cursor_byte_idx)
    {
        return None;
    }

    let expected_end = position_replace.checked_add(base_word.len())?;
    let replace_end = if expected_end <= input.len()
        && input.is_char_boundary(expected_end)
        && input.get(position_replace..expected_end) == Some(base_word)
    {
        expected_end
    } else {
        cursor_byte_idx
    };
    let suffix = input.get(replace_end..)?;

    let mut completed = input[..position_replace].to_owned();
    completed.push_str(matched);
    let cursor_byte = if add_space {
        if suffix.starts_with(' ') {
            completed.len() + 1
        } else {
            completed.push(' ');
            completed.len()
        }
    } else {
        completed.len()
    };
    completed.push_str(suffix);
    let cursor_char = completed[..cursor_byte].chars().count();

    Some((completed, cursor_char))
}

fn selected_text(text: &str, start: usize, end: usize) -> String {
    let start = char_to_byte(text, start);
    let end = char_to_byte(text, end);
    text[start..end].to_owned()
}

fn contains_complete_mention(text: &str, label: &str) -> bool {
    text.match_indices(label).any(|(start, matched)| {
        let before = text[..start].chars().next_back();
        let after = text[start + matched.len()..].chars().next();
        let continues_identifier = |character: char| {
            character.is_alphanumeric() || matches!(character, '_' | '-' | ':' | '.')
        };
        before.is_none_or(|character| {
            !continues_identifier(character) && character != '@'
        }) && after.is_none_or(|character| !continues_identifier(character))
    })
}

fn replace_selection(text: &mut String, start: usize, end: usize, replacement: &str) -> usize {
    let start_byte = char_to_byte(text, start);
    let end_byte = char_to_byte(text, end);
    text.replace_range(start_byte..end_byte, replacement);
    start + replacement.chars().count()
}

fn store_cursor(ctx: &egui::Context, id: egui::Id, range: CCursorRange) {
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(range));
    state.store(ctx, id);
    ctx.memory_mut(|memory| memory.request_focus(id));
}

fn restore_undo_state(ctx: &egui::Context, id: egui::Id, text: &mut String, redo: bool) -> bool {
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let current_range = state
        .cursor
        .char_range()
        .unwrap_or_else(|| CCursorRange::one(CCursor::new(text.chars().count())));
    let current = (current_range, text.clone());
    let mut undoer = state.undoer();
    let restored = if redo {
        undoer.redo(&current).cloned()
    } else {
        undoer.undo(&current).cloned()
    };
    state.set_undoer(undoer);

    let Some((range, restored_text)) = restored else {
        return false;
    };
    *text = restored_text;
    state.cursor.set_char_range(Some(range));
    state.store(ctx, id);
    ctx.memory_mut(|memory| memory.request_focus(id));
    true
}

/// Add the conventional desktop edit menu to a chat input. Keeping this in one
/// helper ensures the room composer and thread composer have identical editing
/// behavior.
pub(crate) fn input_context_menu(response: &egui::Response, text: &mut String) {
    response.context_menu(|ui| {
        let ctx = ui.ctx().clone();
        let id = response.id;
        let (start, end) = selected_char_range(&ctx, id, text);
        let has_selection = start != end;
        let char_count = text.chars().count();
        let current_range = TextEditState::load(&ctx, id)
            .and_then(|state| state.cursor.char_range())
            .unwrap_or_else(|| CCursorRange::one(CCursor::new(char_count)));
        let current = (current_range, text.clone());
        let undoer = TextEditState::load(&ctx, id)
            .map(|state| state.undoer())
            .unwrap_or_default();

        if ui
            .add_enabled(
                undoer.has_undo(&current),
                egui::Button::new("Undo").shortcut_text("Ctrl+Z"),
            )
            .clicked()
        {
            restore_undo_state(&ctx, id, text, false);
            ui.close_menu();
        }
        if ui
            .add_enabled(
                undoer.has_redo(&current),
                egui::Button::new("Redo").shortcut_text("Ctrl+Shift+Z"),
            )
            .clicked()
        {
            restore_undo_state(&ctx, id, text, true);
            ui.close_menu();
        }

        ui.separator();

        if ui
            .add_enabled(
                has_selection,
                egui::Button::new("Cut").shortcut_text("Ctrl+X"),
            )
            .clicked()
        {
            ctx.copy_text(selected_text(text, start, end));
            let cursor = replace_selection(text, start, end, "");
            store_cursor(&ctx, id, CCursorRange::one(CCursor::new(cursor)));
            ui.close_menu();
        }
        if ui
            .add_enabled(
                has_selection,
                egui::Button::new("Copy").shortcut_text("Ctrl+C"),
            )
            .clicked()
        {
            ctx.copy_text(selected_text(text, start, end));
            ctx.memory_mut(|memory| memory.request_focus(id));
            ui.close_menu();
        }

        let clipboard_text = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.get_text())
            .ok()
            .filter(|contents| !contents.is_empty());
        if ui
            .add_enabled(
                clipboard_text.is_some(),
                egui::Button::new("Paste").shortcut_text("Ctrl+V"),
            )
            .clicked()
        {
            if let Some(clipboard_text) = clipboard_text {
                let cursor = replace_selection(text, start, end, &clipboard_text);
                store_cursor(&ctx, id, CCursorRange::one(CCursor::new(cursor)));
            }
            ui.close_menu();
        }
        if ui.add_enabled(has_selection, egui::Button::new("Delete")).clicked() {
            let cursor = replace_selection(text, start, end, "");
            store_cursor(&ctx, id, CCursorRange::one(CCursor::new(cursor)));
            ui.close_menu();
        }

        ui.separator();

        let all_selected = start == 0 && end == char_count && char_count > 0;
        if ui
            .add_enabled(
                char_count > 0 && !all_selected,
                egui::Button::new("Select All").shortcut_text("Ctrl+A"),
            )
            .clicked()
        {
            store_cursor(
                &ctx,
                id,
                CCursorRange::two(CCursor::new(0), CCursor::new(char_count)),
            );
            ui.close_menu();
        }
    });
}

fn matrix_reply_command(event_id: &str, message: &str) -> String {
    format!("/reply {} {}", event_id, message)
}

fn char_to_byte_idx(text: &str, char_idx: usize) -> usize {
    text.char_indices().nth(char_idx).map(|(idx, _)| idx).unwrap_or(text.len())
}

fn mention_query(text: &str, cursor_char_idx: usize) -> Option<(usize, usize, &str)> {
    let cursor_byte_idx = char_to_byte_idx(text, cursor_char_idx);
    let before_cursor = &text[..cursor_byte_idx];
    let trigger_byte_idx = before_cursor.char_indices().rev().find_map(|(idx, ch)| {
        if ch != '@' { return None; }
        let valid_boundary = idx == 0 || before_cursor[..idx].chars().next_back()
            .map(|previous| previous.is_whitespace() || matches!(previous, '(' | '[' | '{' | '"' | '\''))
            .unwrap_or(true);
        valid_boundary.then_some(idx)
    })?;
    let query = &before_cursor[trigger_byte_idx + '@'.len_utf8()..];
    if query.starts_with(char::is_whitespace) || query.chars().count() > 80 {
        return None;
    }
    Some((trigger_byte_idx, cursor_byte_idx, query))
}

fn matching_mentions(candidates: &[MentionCandidate], query: &str) -> Vec<MentionCandidate> {
    let query = query.to_lowercase();
    let mut matches: Vec<_> = candidates.iter()
        .filter(|candidate| candidate.display_name.to_lowercase().contains(&query)
            || candidate.user_id.to_lowercase().contains(&query))
        .cloned()
        .collect();
    matches.sort_by_key(|candidate| {
        let name = candidate.display_name.to_lowercase();
        let id = candidate.user_id.to_lowercase();
        (!name.starts_with(&query), !id.trim_start_matches('@').starts_with(&query), name, id)
    });

    if query.is_empty() {
        let mut homeservers: Vec<(String, Vec<MentionCandidate>)> = Vec::new();
        for candidate in matches {
            let homeserver = candidate.user_id.rsplit_once(':')
                .map(|(_, homeserver)| homeserver)
                .unwrap_or("")
                .to_owned();
            if let Some((_, candidates)) = homeservers.iter_mut()
                .find(|(existing, _)| existing == &homeserver)
            {
                candidates.push(candidate);
            } else {
                homeservers.push((homeserver, vec![candidate]));
            }
        }

        let mut diversified = Vec::new();
        let mut round = 0;
        while diversified.len() < 8 {
            let before = diversified.len();
            for (_, candidates) in &homeservers {
                if let Some(candidate) = candidates.get(round) {
                    diversified.push(candidate.clone());
                    if diversified.len() == 8 { break; }
                }
            }
            if diversified.len() == before { break; }
            round += 1;
        }
        return diversified;
    }

    matches.truncate(8);
    matches
}

impl WeeChatApp {
    pub(crate) fn request_command_completion(&mut self, ctx: &egui::Context, id: egui::Id) {
        let cursor_char_idx = TextEditState::load(ctx, id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| range.primary.index)
            .unwrap_or_else(|| self.input_text.chars().count());
        let cursor_byte_idx = char_to_byte(&self.input_text, cursor_char_idx);

        if !self.input_text.starts_with('/') {
            self.command_completion = None;
            self.command_completion_pending = None;
            return;
        }
        // Slash commands use the backend-native completer for commands and
        // arguments, so do not leave the separate @-mention popup competing
        // for the same input and navigation keys.
        self.mention_completion = None;
        let Some(buffer_id) = self.selected_buffer_id.clone() else {
            self.command_completion = None;
            self.command_completion_pending = None;
            return;
        };
        if self.command_completion_pending.as_ref().is_some_and(|pending| {
            pending.buffer_id == buffer_id
                && pending.input == self.input_text
                && pending.cursor_byte_idx == cursor_byte_idx
        }) {
            return;
        }

        self.command_completion_request_seq =
            self.command_completion_request_seq.wrapping_add(1);
        let sequence = self.command_completion_request_seq;
        let input = self.input_text.clone();
        let sent = self
            .client_for_buffer(&buffer_id)
            .is_some_and(|(client, raw_id)| {
                client.request_completion(&raw_id, &input, cursor_byte_idx, sequence)
            });
        self.command_completion = None;
        self.command_completion_pending = sent.then_some(CommandCompletionRequest {
            sequence,
            buffer_id,
            input,
            cursor_byte_idx,
        });
    }

    pub(crate) fn move_command_completion_selection(&mut self, delta: isize) {
        let Some(state) = &mut self.command_completion else {
            return;
        };
        state.index =
            (state.index as isize + delta).rem_euclid(state.matches.len() as isize) as usize;
    }

    pub(crate) fn accept_command_completion(
        &mut self,
        index: Option<usize>,
        ctx: &egui::Context,
        id: egui::Id,
    ) {
        let Some(state) = self.command_completion.take() else {
            return;
        };
        let index = index.unwrap_or(state.index).min(state.matches.len() - 1);
        let Some((new_text, cursor_char_idx)) = apply_native_completion(
            &state.source_text,
            state.cursor_byte_idx,
            state.position_replace,
            &state.base_word,
            &state.matches[index],
            state.add_space,
        ) else {
            return;
        };

        self.input_text = new_text;
        self.command_completion_pending = None;
        if let Some(mut edit_state) = TextEditState::load(ctx, id) {
            edit_state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(cursor_char_idx))));
            edit_state.store(ctx, id);
        }
        self.request_command_completion(ctx, id);
    }

    pub(crate) fn refresh_mention_completion(&mut self, ctx: &egui::Context, id: egui::Id) {
        let cursor_char_idx = TextEditState::load(ctx, id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| range.primary.index)
            .unwrap_or_else(|| self.input_text.chars().count());
        let Some((trigger_byte_idx, cursor_byte_idx, query)) =
            mention_query(&self.input_text, cursor_char_idx)
        else {
            self.mention_completion = None;
            return;
        };
        let Some(buffer) = self.selected_buffer_id.as_ref()
            .and_then(|buffer_id| self.buffers.iter().find(|buffer| &buffer.id == buffer_id))
        else {
            self.mention_completion = None;
            return;
        };
        let fallback_candidates;
        let candidates = if buffer.mention_candidates.is_empty() {
            fallback_candidates = buffer.nicks.iter().map(|nick| MentionCandidate {
                display_name: nick.name.clone(),
                user_id: nick.name.clone(),
            }).collect::<Vec<_>>();
            fallback_candidates.as_slice()
        } else {
            buffer.mention_candidates.as_slice()
        };
        let matches = matching_mentions(candidates, query);
        if matches.is_empty() {
            self.mention_completion = None;
            return;
        }
        let previous_index = self.mention_completion.as_ref()
            .map(|state| state.index)
            .unwrap_or(0);
        self.mention_completion = Some(MentionCompletionState {
            trigger_byte_idx,
            cursor_byte_idx,
            index: previous_index.min(matches.len() - 1),
            matches,
        });
    }

    pub(crate) fn move_mention_selection(&mut self, delta: isize) {
        let Some(state) = &mut self.mention_completion else { return; };
        state.index =
            (state.index as isize + delta).rem_euclid(state.matches.len() as isize) as usize;
    }

    pub(crate) fn accept_mention(&mut self, index: Option<usize>, ctx: &egui::Context, id: egui::Id) {
        let Some(state) = self.mention_completion.take() else { return; };
        let index = index.unwrap_or(state.index).min(state.matches.len() - 1);
        let candidate = &state.matches[index];
        let label = if candidate.display_name.starts_with('@') {
            candidate.display_name.clone()
        } else {
            format!("@{}", candidate.display_name)
        };
        let mut new_text = self.input_text[..state.trigger_byte_idx].to_owned();
        new_text.push_str(&label);
        new_text.push(' ');
        let cursor_char_idx = new_text.chars().count();
        new_text.push_str(&self.input_text[state.cursor_byte_idx..]);
        self.input_text = new_text;
        self.selected_mentions.retain(|mention| {
            mention.user_id != candidate.user_id && mention.label != label
        });
        self.selected_mentions.push(SelectedMention {
            label,
            user_id: candidate.user_id.clone(),
        });
        if let Some(mut edit_state) = TextEditState::load(ctx, id) {
            edit_state.cursor.set_char_range(Some(CCursorRange::one(
                CCursor::new(cursor_char_idx),
            )));
            edit_state.store(ctx, id);
        }
    }

    pub(crate) fn reconcile_selected_mentions(&mut self) {
        self.selected_mentions.retain(|mention| {
            contains_complete_mention(&self.input_text, &mention.label)
        });
    }

    pub(crate) fn perform_completion(&mut self, ctx: &egui::Context, id: egui::Id) {
        if self.input_text.starts_with('/') {
            if self.command_completion.is_some() {
                self.accept_command_completion(None, ctx, id);
            } else {
                self.request_command_completion(ctx, id);
            }
            return;
        }

        let mut new_cursor_char = 0usize;

        if let Some(state) = &mut self.completion {
            // Cycle to next match
            if state.matches.is_empty() { return; }
            state.index = (state.index + 1) % state.matches.len();
            let matched = state.matches[state.index].clone();

            let mut new_text = self.input_text[..state.word_start_idx].to_string();
            new_text.push_str(&matched);
            // Nick at line start gets ": ", emoji and mid-line nicks get " "
            if !state.original_word.starts_with(':') && state.word_start_idx == 0 {
                new_text.push_str(": ");
            } else {
                new_text.push(' ');
            }
            new_cursor_char = new_text.chars().count();
            self.input_text = new_text;
        } else {
            // Start new completion
            let word_start = self.input_text.rfind(' ').map(|i| i + 1).unwrap_or(0);
            let word = self.input_text[word_start..].to_string();
            if word.is_empty() { return; }

            let matches: Vec<String> = if word.starts_with(':') && word.len() >= 2 {
                emoji::find_matches(&word[1..])
            } else {
                match self.selected_buffer_id.as_ref()
                    .and_then(|bid| self.buffers.iter().find(|b| &b.id == bid))
                {
                    Some(buf) => buf.nicks.iter()
                        .filter(|n| n.name.to_lowercase().starts_with(&word.to_lowercase()))
                        .map(|n| n.name.clone())
                        .collect(),
                    None => return,
                }
            };

            if !matches.is_empty() {
                let matched = matches[0].clone();
                let mut new_text = self.input_text[..word_start].to_string();
                new_text.push_str(&matched);
                if !word.starts_with(':') && word_start == 0 {
                    new_text.push_str(": ");
                } else {
                    new_text.push(' ');
                }
                new_cursor_char = new_text.chars().count();
                self.input_text = new_text;
                self.completion = Some(CompletionState {
                    original_word: word,
                    matches,
                    index: 0,
                    word_start_idx: word_start,
                });
            }
        }

        if new_cursor_char > 0 {
            if let Some(mut state) = TextEditState::load(ctx, id) {
                state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_cursor_char))));
                state.store(ctx, id);
            }
        }
    }

    pub(crate) fn cycle_history(&mut self, delta: i32, ctx: &egui::Context, id: egui::Id) {
        if self.command_history.is_empty() { return; }

        let new_index = match self.history_index {
            Some(idx) => {
                if delta < 0 && idx == 0 {
                    Some(0)
                } else {
                    let next = idx as i32 + delta;
                    if next >= self.command_history.len() as i32 {
                        None
                    } else {
                        Some(next.max(0) as usize)
                    }
                }
            }
            None => {
                if delta < 0 { Some(self.command_history.len() - 1) } else { None }
            }
        };

        self.history_index = new_index;
        if let Some(idx) = self.history_index {
            self.input_text = self.command_history[idx].clone();
        } else {
            self.input_text.clear();
        }

        let pos = self.input_text.len();
        if let Some(mut state) = TextEditState::load(ctx, id) {
            state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(pos))));
            state.store(ctx, id);
        }
    }

    pub(crate) fn cycle_buffer(&mut self, delta: i32) {
        let navigable: Vec<String> = self.buffers.iter()
            .filter(|buffer| !buffer.is_matrix_thread())
            .map(|buffer| buffer.id.clone())
            .collect();
        if navigable.is_empty() { return; }
        let current_id = match self.selected_buffer_id.clone() {
            Some(id) => id,
            None => {
                if let Some(first) = navigable.first() {
                    let id = first.clone();
                    self.select_buffer(id);
                }
                return;
            }
        };

        if let Some(idx) = navigable.iter().position(|id| id == &current_id) {
            let new_idx = (idx as i32 + delta).rem_euclid(navigable.len() as i32) as usize;
            let new_id = navigable[new_idx].clone();
            self.select_buffer(new_id);
        }
    }

    /// Jump to the Nth visible (non-hidden) buffer (1-indexed).
    pub(crate) fn jump_buffer_by_number(&mut self, n: usize) {
        if n == 0 { return; }
        let visible: Vec<String> = self.buffers.iter()
            .filter(|b| !b.is_matrix_thread() && (!b.hidden || self.show_hidden_buffers))
            .map(|b| b.id.clone())
            .collect();
        if let Some(id) = visible.get(n - 1) {
            let id = id.clone();
            self.select_buffer(id);
        }
    }

    /// Jump to the next buffer that has unread messages or highlights, wrapping around.
    pub(crate) fn jump_next_unread(&mut self) {
        use crate::relay::models::BufferActivity;
        if self.buffers.is_empty() { return; }
        let start = self.selected_buffer_id.as_ref()
            .and_then(|id| self.buffers.iter().position(|b| &b.id == id))
            .unwrap_or(0);
        let len = self.buffers.len();
        for i in 1..=len {
            let idx = (start + i) % len;
            let buf = &self.buffers[idx];
            if buf.is_matrix_thread() { continue; }
            if buf.hidden && !self.show_hidden_buffers { continue; }
            if buf.activity != BufferActivity::None || buf.unread_count > 0 {
                let id = buf.id.clone();
                self.select_buffer(id);
                return;
            }
        }
    }

    pub(crate) fn send_thread_message(&mut self) {
        let message = self.thread_input_text.trim().to_owned();
        if message.is_empty() {
            return;
        }
        let Some(buffer_id) = self.open_thread_buffer_id.clone() else {
            return;
        };
        if !self
            .buffer_by_id(&buffer_id)
            .is_some_and(|buffer| buffer.is_matrix_thread())
        {
            return;
        }
        if let Some((client, raw_id)) = self.client_for_buffer(&buffer_id) {
            client.send_message(&raw_id, &message);
            self.thread_input_text.clear();
        }
    }

    pub(crate) fn send_current_message(&mut self) {
        if self.input_text.is_empty() { return; }
        let msg = self.input_text.clone();

        let is_command = msg.starts_with('/');
        let sends_reply = self.reply_target.as_ref().is_some_and(|reply| {
            self.selected_buffer_id.as_deref() == Some(reply.buffer_id.as_str())
        });

        // Determine pending_buffer_switch before the borrow via client_for_buffer
        if is_command && !sends_reply {
            if msg.starts_with("/query ") {
                self.pending_buffer_switch = msg[7..].split_whitespace().next().map(|s| s.to_string());
            } else if msg.starts_with("/join ") || msg.starts_with("/j ") {
                let after = if msg.starts_with("/j ") { &msg[3..] } else { &msg[6..] };
                self.pending_buffer_switch = after.split_whitespace().next().map(|s| s.to_string());
            } else if msg.trim() == "/sysinfo" || msg.trim() == "/systeminfo" {
                if let Some(buffer_id) = self.selected_buffer_id.clone() {
                    let tx = self.sysinfo_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let info = crate::ui::sysinfo::gather();
                        let text = crate::ui::sysinfo::format_line(&info);
                        let _ = tx.send((buffer_id, text));
                    });
                }
                self.input_text.clear();
                self.completion = None;
                self.command_completion = None;
                self.command_completion_pending = None;
                self.history_index = None;
                return;
            } else if msg.trim() == "/np" || msg.starts_with("/np ") {
                if let Some(buffer_id) = self.selected_buffer_id.clone() {
                    let custom = msg.trim().strip_prefix("/np").map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| s.to_string());
                    let tx = self.np_tx.clone();
                    tokio::spawn(async move {
                        let text = if let Some(c) = custom {
                            format!("{} 🎵", c)
                        } else if let Some(np) = crate::ui::np::get_now_playing().await {
                            let mut s = np.track;
                            if !np.source.is_empty() {
                                s.push_str(&format!(" @ {}", np.source));
                            }
                            s.push_str(" 🎵");
                            if !np.url.is_empty() {
                                s.push_str(&format!(" {}", np.url));
                            }
                            s
                        } else {
                            return;
                        };
                        let _ = tx.send((buffer_id, text));
                    });
                }
                self.input_text.clear();
                self.completion = None;
                self.command_completion = None;
                self.command_completion_pending = None;
                self.history_index = None;
                return;
            }
        }

        if let Some(buffer_id) = self.selected_buffer_id.clone() {
            let semantic_matrix_message = !is_command
                && !sends_reply
                && !self.selected_mentions.is_empty()
                && self.buffer_by_id(&buffer_id)
                    .map(|buffer| buffer.plugin == "matrix")
                    .unwrap_or(false);
            let command = if semantic_matrix_message {
                let payload = serde_json::json!({
                    "body": msg,
                    "user_ids": self.selected_mentions.iter()
                        .map(|mention| mention.user_id.as_str())
                        .collect::<Vec<_>>(),
                });
                format!(
                    "/matrix-send {}",
                    URL_SAFE_NO_PAD.encode(payload.to_string()),
                )
            } else {
                msg.clone()
            };
            if let Some((client, raw_id)) = self.client_for_buffer(&buffer_id) {
                let command = self
                    .reply_target
                    .as_ref()
                    .filter(|reply| reply.buffer_id == buffer_id)
                    .map(|reply| matrix_reply_command(&reply.matrix_event_id, &msg))
                    .unwrap_or(command);
                client.send_message(&raw_id, &command);
                if is_command && !sends_reply {
                    client.fetch_buffer_list();
                }
            }
        }

        if self.command_history.back().map(|s| s.as_str()) != Some(&msg) {
            self.command_history.push_back(msg);
            if self.command_history.len() > 100 {
                self.command_history.pop_front();
            }
        }

        self.input_text.clear();
        self.completion = None;
        self.command_completion = None;
        self.command_completion_pending = None;
        self.reply_target = None;
        self.mention_completion = None;
        self.selected_mentions.clear();
        self.history_index = None;
    }

    pub(crate) fn send_command(&mut self, command: &str) {
        if command.starts_with("/query ") {
            self.pending_buffer_switch = Some(command[7..].trim().to_string());
        } else if command.starts_with("/join ") {
            self.pending_buffer_switch = Some(command[6..].trim().to_string());
        }

        if let Some(buffer_id) = self.selected_buffer_id.clone() {
            if let Some((client, raw_id)) = self.client_for_buffer(&buffer_id) {
                client.send_message(&raw_id, command);
                client.fetch_buffer_list();
            }
        }
    }

    pub(crate) fn send_command_to_buffer(&mut self, buffer_id: &str, command: &str) {
        if let Some((client, raw_id)) = self.client_for_buffer(buffer_id) {
            client.send_message(&raw_id, command);
            client.fetch_buffer_list();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_native_completion, contains_complete_mention, matching_mentions,
        matrix_reply_command, mention_query, replace_selection, selected_text,
    };
    use crate::relay::models::MentionCandidate;

    fn candidate(display_name: &str, user_id: &str) -> MentionCandidate {
        MentionCandidate {
            display_name: display_name.to_owned(),
            user_id: user_id.to_owned(),
        }
    }

    #[test]
    fn reply_command_keeps_the_selected_matrix_event() {
        assert_eq!(
            matrix_reply_command("$chosen:elsewhere.example", "not the latest"),
            "/reply $chosen:elsewhere.example not the latest"
        );
    }

    #[test]
    fn native_command_completion_keeps_the_slash_and_adds_a_space() {
        assert_eq!(
            apply_native_completion("/qu", 3, 1, "qu", "query", true),
            Some(("/query ".to_owned(), 7)),
        );
    }

    #[test]
    fn native_argument_completion_replaces_only_the_server_selected_word() {
        assert_eq!(
            apply_native_completion(
                "/join #po trailing",
                9,
                6,
                "#po",
                "#postgis",
                true,
            ),
            Some(("/join #postgis trailing".to_owned(), 15)),
        );
    }

    #[test]
    fn mention_query_starts_at_at_sign_and_keeps_unicode_cursor() {
        assert_eq!(mention_query("Привет @Ада", 11), Some((13, 20, "Ада")));
    }

    #[test]
    fn mention_query_does_not_open_inside_an_email_address() {
        assert_eq!(mention_query("mail@example.org", 16), None);
    }

    #[test]
    fn mention_matches_display_name_and_matrix_id() {
        let candidates = vec![
            candidate("Ada Lovelace", "@ada:example.org"),
            candidate("Grace Hopper", "@amazing:example.org"),
        ];
        assert_eq!(
            matching_mentions(&candidates, "ada"),
            vec![candidate("Ada Lovelace", "@ada:example.org")],
        );
        assert_eq!(
            matching_mentions(&candidates, "amazing"),
            vec![candidate("Grace Hopper", "@amazing:example.org")],
        );
    }

    #[test]
    fn semantic_mentions_require_the_complete_accepted_label() {
        assert!(contains_complete_mention("hi @Ada!", "@Ada"));
        assert!(contains_complete_mention("(@Ada Lovelace)", "@Ada Lovelace"));
        assert!(!contains_complete_mention("hi @Adam", "@Ada"));
        assert!(!contains_complete_mention("hi @Ada_example", "@Ada"));
        assert!(!contains_complete_mention("mail@Ada", "@Ada"));
    }

    #[test]
    fn context_menu_selection_uses_character_offsets() {
        let mut text = "aб😺中z".to_owned();
        assert_eq!(selected_text(&text, 1, 4), "б😺中");

        let cursor = replace_selection(&mut text, 1, 4, "🌍");
        assert_eq!(text, "a🌍z");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn empty_matrix_query_represents_federated_homeservers() {
        let candidates = vec![
            candidate("Ada", "@ada:matrix.org"),
            candidate("Bob", "@bob:matrix.org"),
            candidate("Cara", "@cara:osgeo.org"),
            candidate("Dan", "@dan:dend.ro"),
        ];
        assert_eq!(
            matching_mentions(&candidates, ""),
            vec![
                candidate("Ada", "@ada:matrix.org"),
                candidate("Cara", "@cara:osgeo.org"),
                candidate("Dan", "@dan:dend.ro"),
                candidate("Bob", "@bob:matrix.org"),
            ],
        );
    }

    #[test]
    fn empty_irc_query_keeps_nicks_alphabetical() {
        let candidates = vec![
            candidate("Zed", "Zed"),
            candidate("alice", "alice"),
        ];
        assert_eq!(
            matching_mentions(&candidates, ""),
            vec![candidate("alice", "alice"), candidate("Zed", "Zed")],
        );
    }
}
