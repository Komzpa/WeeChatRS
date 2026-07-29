use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use egui::text_edit::TextEditState;
use egui::text::{CCursorRange, CCursor};
use crate::relay::models::MentionCandidate;
use crate::ui::app::{
    WeeChatApp, CompletionState, MentionCompletionState, SelectedMention,
};
use crate::ui::emoji;

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
        self.selected_mentions.retain(|mention| self.input_text.contains(&mention.label));
    }

    pub(crate) fn perform_completion(&mut self, ctx: &egui::Context, id: egui::Id) {
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
            let semantic_matrix_message = !is_command
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
                client.send_message(&raw_id, &command);
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
    use super::{matching_mentions, mention_query};
    use crate::relay::models::MentionCandidate;

    fn candidate(display_name: &str, user_id: &str) -> MentionCandidate {
        MentionCandidate {
            display_name: display_name.to_owned(),
            user_id: user_id.to_owned(),
        }
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
