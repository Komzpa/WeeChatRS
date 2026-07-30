use egui::text_edit::TextEditState;
use egui::text::{CCursorRange, CCursor};
use crate::ui::app::{CommandCompletionRequest, CompletionState, WeeChatApp};
use crate::ui::emoji;

fn char_to_byte_idx(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
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

impl WeeChatApp {
    pub(crate) fn request_command_completion(&mut self, ctx: &egui::Context, id: egui::Id) {
        let cursor_char_idx = TextEditState::load(ctx, id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| range.primary.index)
            .unwrap_or_else(|| self.input_text.chars().count());
        let cursor_byte_idx = char_to_byte_idx(&self.input_text, cursor_char_idx);

        if !self.input_text.starts_with('/') {
            self.command_completion = None;
            self.command_completion_pending = None;
            return;
        }
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
        if self.buffers.is_empty() { return; }
        let current_id = match self.selected_buffer_id.clone() {
            Some(id) => id,
            None => {
                if let Some(first) = self.buffers.first() {
                    let id = first.id.clone();
                    self.select_buffer(id);
                }
                return;
            }
        };

        if let Some(idx) = self.buffers.iter().position(|b| b.id == current_id) {
            let new_idx = (idx as i32 + delta).rem_euclid(self.buffers.len() as i32) as usize;
            let new_id = self.buffers[new_idx].id.clone();
            self.select_buffer(new_id);
        }
    }

    /// Jump to the Nth visible (non-hidden) buffer (1-indexed).
    pub(crate) fn jump_buffer_by_number(&mut self, n: usize) {
        if n == 0 { return; }
        let visible: Vec<String> = self.buffers.iter()
            .filter(|b| !b.hidden || self.show_hidden_buffers)
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
            if buf.hidden && !self.show_hidden_buffers { continue; }
            if buf.activity != BufferActivity::None || buf.unread_count > 0 {
                let id = buf.id.clone();
                self.select_buffer(id);
                return;
            }
        }
    }

    pub(crate) fn send_current_message(&mut self) {
        if self.input_text.is_empty() { return; }
        let msg = self.input_text.clone();

        let is_command = msg.starts_with('/');

        // Determine pending_buffer_switch before the borrow via client_for_buffer
        if is_command {
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
                self.history_index = None;
                return;
            }
        }

        if let Some(buffer_id) = self.selected_buffer_id.clone() {
            if let Some((client, raw_id)) = self.client_for_buffer(&buffer_id) {
                client.send_message(&raw_id, &msg);
                if is_command {
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
    use super::apply_native_completion;

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
}
