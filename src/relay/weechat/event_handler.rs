use crate::relay::backend::BackendEvent;
use crate::relay::models::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use crate::ui::app::{
    history_snapshot_is_exhausted, is_restorable_chat_buffer, preferred_chat_buffer_id,
    CommandCompletionState, SavedReadMarker, WeeChatApp, LOAD_MORE_LINES, MAX_STORED_LINES,
};
use chrono::{Utc, DateTime, Local};
use serde_json::Value;
use std::sync::OnceLock;

static ANSI_RE: OnceLock<regex::Regex> = OnceLock::new();

fn ansi_re() -> &'static regex::Regex {
    ANSI_RE.get_or_init(|| regex::Regex::new(r"\x1B\[[0-9;]*[A-Za-z]").unwrap())
}

fn matrix_history_page_status(message: &str) -> Option<(usize, bool)> {
    let mut fields = message.split_ascii_whitespace();
    (fields.next()? == "matrix_history_page").then_some(())?;
    let mut added = None;
    let mut exhausted = None;
    for field in fields {
        if let Some(value) = field.strip_prefix("added=") {
            added = value.parse::<usize>().ok();
        } else if let Some(value) = field.strip_prefix("exhausted=") {
            exhausted = match value {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            };
        }
    }
    Some((added?, exhausted?))
}

fn sort_lines_chronologically(lines: &mut [Line]) {
    // WeeChat's ordinary buffers arrive oldest-first, while Matrix history can
    // arrive newest-first or as a descending history block followed by newer
    // live lines. Stable sorting keeps equal-timestamp reply/media fragments in
    // their relay order while giving every downstream path one invariant.
    lines.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
}

fn buffer_group_key(buffer: &Buffer) -> (String, String) {
    let connection = buffer.id.split('/').next().unwrap_or_default();
    (connection.to_owned(), buffer.server.clone())
}

fn buffer_groups_are_valid(buffers: &[Buffer]) -> bool {
    let mut seen_groups = std::collections::HashSet::new();
    let mut current_group: Option<(String, String)> = None;
    let mut current_group_has_child = false;

    for buffer in buffers.iter().filter(|buffer| !buffer.is_matrix_thread()) {
        let group = buffer_group_key(buffer);
        if current_group.as_ref() != Some(&group) {
            if !seen_groups.insert(group.clone()) {
                return false;
            }
            current_group = Some(group);
            current_group_has_child = false;
        }

        let is_root = buffer.kind == "server" || buffer.kind == "core";
        if is_root && current_group_has_child {
            return false;
        }
        current_group_has_child |= !is_root;
    }

    true
}

/// Apply a saved visual order without allowing stale IDs to tear server groups
/// apart. Returns false after restoring the already-grouped fallback order.
fn apply_saved_buffer_order(buffers: &mut [Buffer], order: &[String]) -> bool {
    if order.is_empty() {
        return true;
    }

    let fallback_positions = buffers
        .iter()
        .enumerate()
        .map(|(position, buffer)| (buffer.id.clone(), position))
        .collect::<std::collections::HashMap<_, _>>();
    let max_order = order.len();
    let server_header_pos = buffers
        .iter()
        .filter(|buffer| buffer.kind == "server" || buffer.kind == "core")
        .filter_map(|buffer| {
            order
                .iter()
                .position(|id| id == &buffer.id)
                .map(|position| (buffer_group_key(buffer), position))
        })
        .collect::<std::collections::HashMap<_, _>>();

    buffers.sort_by_key(|buffer| {
        if let Some(position) = order.iter().position(|id| id == &buffer.id) {
            position * 10_000
        } else {
            let base = server_header_pos
                .get(&buffer_group_key(buffer))
                .map(|position| position * 10_000 + 1)
                .unwrap_or(max_order * 10_000 + 1);
            base + buffer.number.max(0) as usize
        }
    });

    if buffer_groups_are_valid(buffers) {
        true
    } else {
        buffers.sort_by_key(|buffer| fallback_positions[&buffer.id]);
        false
    }
}

#[cfg(test)]
mod matrix_media_tests {
    use super::WeeChatApp;
    use serde_json::json;

    #[test]
    fn matrix_image_metadata_is_read_from_structured_tags() {
        let object = json!({
            "tags": [
                "matrix_media",
                "matrix_media_kind_image",
                "matrix_media_name_aW1hZ2UucG5n",
                "matrix_media_uri_bXhjOi8vbWF0cml4Lm9yZy9zb21lLW1lZGlhLWlk"
            ]
        });
        let media =
            WeeChatApp::matrix_media_from_tags(object.as_object().unwrap())
                .expect("valid image metadata");
        assert_eq!(media.kind, "image");
        assert_eq!(media.name, "image.png");
        assert_eq!(media.mxc_uri, "mxc://matrix.org/some-media-id");
    }

    #[test]
    fn non_mxc_matrix_media_uri_is_rejected() {
        let object = json!({
            "tags": [
                "matrix_media",
                "matrix_media_kind_image",
                "matrix_media_name_aW1hZ2UucG5n",
                "matrix_media_uri_aHR0cHM6Ly9leGFtcGxlLm9yZy9pbWFnZS5wbmc"
            ]
        });
        assert!(
            WeeChatApp::matrix_media_from_tags(object.as_object().unwrap())
                .is_none()
        );
    }
}


impl WeeChatApp {
    pub(crate) fn handle_event(&mut self, conn_prefix: &str, event: BackendEvent) {
        match event {
            BackendEvent::Connected => {
                if let Some(conn) = self.connections.iter_mut().find(|c| c.prefix == conn_prefix) {
                    conn.is_connecting = false;
                    conn.connecting_pending = false;
                    conn.auth_error = None;
                    conn.status = "Connected".to_string();
                    match conn.backend_type {
                        crate::ui::app::BackendType::WeeChat => {
                            let ts = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  WebSocket handshake complete", ts));
                            let ts2 = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  Sending credentials via Sec-WebSocket-Protocol bearer token", ts2));
                            let ts3 = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  Authentication accepted by relay", ts3));
                            let ts4 = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  → GET /api/buffers", ts4));
                            let ts5 = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  → POST /api/sync  {{colors: ansi, input: false}}", ts5));
                        }
                        crate::ui::app::BackendType::Soju => {
                            let ts = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  IRC registration complete (CAP/NICK/USER)", ts));
                        }
                    }
                    let ts = chrono::Local::now().format("%H:%M").to_string();
                    conn.connection_log.push_back(format!("[{}]  Connected", ts));
                    if conn.connection_log.len() > 500 { conn.connection_log.pop_front(); }
                }
                // Remove stale buffers for this connection then re-fetch
                let pfx = format!("{}/", conn_prefix);
                self.buffers.retain(|b| !b.id.starts_with(&pfx));
                self.rebuild_buffer_idx();
                // Clear suppression set so the fresh hotlist can apply unread counts correctly
                self.cleared_buffer_ids.retain(|id| !id.starts_with(&pfx));
                // Fetch buffer list and sync subscriptions on connected connection
                let conn_prefix_owned = conn_prefix.to_string();
                if let Some(conn) = self.connections.iter().find(|c| c.prefix == conn_prefix_owned) {
                    conn.client.fetch_buffer_list();
                    conn.client.sync_subscriptions();
                }
                if !self.show_connection_log {
                    self.connection_log_unread = true;
                }
            }
            BackendEvent::Disconnected => {
                if let Some(conn) = self.connections.iter_mut().find(|c| c.prefix == conn_prefix) {
                    conn.is_connecting = false;
                    if conn.connecting_pending {
                        conn.connecting_pending = false;
                        conn.auth_error = Some("Connection closed before auth completed — check your password and relay settings.".to_string());
                        let ts = chrono::Local::now().format("%H:%M").to_string();
                        conn.connection_log.push_back(format!("[{}]  WebSocket closed by server before authentication completed", ts));
                        let ts2 = chrono::Local::now().format("%H:%M").to_string();
                        conn.connection_log.push_back(format!("[{}]  Check: relay password, relay plugin loaded, port correct", ts2));
                        conn.status = String::new();
                        if conn.connection_log.len() > 500 { conn.connection_log.pop_front(); }
                    } else {
                        conn.status = "Disconnected".to_string();
                        if conn.auto_reconnect {
                            let ts = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  Disconnected — auto-reconnect is ON, will retry", ts));
                        } else {
                            let ts = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  Disconnected", ts));
                        }
                        if conn.connection_log.len() > 500 { conn.connection_log.pop_front(); }
                    }
                }
                // Remove buffers for this connection
                let pfx = format!("{}/", conn_prefix);
                self.buffers.retain(|b| !b.id.starts_with(&pfx));
                self.rebuild_buffer_idx();
                if let Some(sel) = &self.selected_buffer_id {
                    if sel.starts_with(&pfx) {
                        self.selected_buffer_id = self.buffers.first().map(|b| b.id.clone());
                    }
                }
                // Remove non-auto-reconnect connections
                let should_remove = self.connections.iter()
                    .find(|c| c.prefix == conn_prefix)
                    .map(|c| !c.auto_reconnect && c.auth_error.is_none())
                    .unwrap_or(false);
                if should_remove {
                    // Keep the handle but mark disconnected so UI shows status
                }
                if !self.show_connection_log {
                    self.connection_log_unread = true;
                }
            }
            BackendEvent::AuthError(e) | BackendEvent::Error(e) => {
                if let Some(conn) = self.connections.iter_mut().find(|c| c.prefix == conn_prefix) {
                    if conn.connecting_pending {
                        let is_auth = e.contains("401") || e.contains("403")
                            || e.to_lowercase().contains("unauthorized")
                            || e.to_lowercase().contains("forbidden");
                        conn.connecting_pending = false;
                        if is_auth {
                            let ts = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  Auth error: {}", ts, e));
                            let ts2 = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  Wrong password or relay not configured to accept this connection", ts2));
                        } else {
                            let ts = chrono::Local::now().format("%H:%M").to_string();
                            conn.connection_log.push_back(format!("[{}]  Connection error: {}", ts, e));
                        }
                        conn.auth_error = Some(if is_auth {
                            "Wrong password or relay not configured to accept this connection.".to_string()
                        } else {
                            format!("Connection failed: {}", e)
                        });
                        conn.status = String::new();
                        if conn.connection_log.len() > 500 { conn.connection_log.pop_front(); }
                    } else {
                        let ts = chrono::Local::now().format("%H:%M").to_string();
                        conn.connection_log.push_back(format!("[{}]  Error: {}", ts, e));
                        conn.status = format!("Error: {}", e);
                        if conn.connection_log.len() > 500 { conn.connection_log.pop_front(); }
                    }
                }
                if !self.show_connection_log {
                    self.connection_log_unread = true;
                }
            }
            BackendEvent::ConnLog(msg) => {
                self.log_conn_for(conn_prefix, msg);
            }
            BackendEvent::_WeeChat(resp) => {
                self.process_response(conn_prefix, resp);
            }
            BackendEvent::BufferOpened(mut buf) => {
                let full_id = format!("{}/{}", conn_prefix, buf.id);
                let full_full_name = format!("{}/{}", conn_prefix, buf.full_name);
                buf.id = full_id.clone();
                buf.full_name = full_full_name;
                if !self.buffer_idx_of(&full_id).is_some() {
                    self.buffers.push(buf);
                    self.rebuild_buffer_idx();
                }
                // Incremental backends can announce a service buffer first. If a
                // remembered chat belongs to this connection, wait for that exact
                // chat instead of replacing the user's restart destination.
                let should_select = self.buffer_by_id(&full_id)
                    .is_some_and(|buffer| {
                        is_restorable_chat_buffer(buffer)
                            && (self.last_chat_buffer_name.is_none()
                                || self.last_chat_buffer_name.as_deref()
                                    == Some(buffer.full_name.as_str()))
                    });
                if self.selected_buffer_id.is_none() && should_select {
                    self.select_buffer(full_id.clone());
                }
                // Resolve a pending /join or /query switch
                if let Some(target) = self.pending_buffer_switch.take() {
                    if let Some(found) = self.buffers.iter().find(|b|
                        b.name == target
                        || b.id == target
                        || b.full_name.ends_with(&target)
                    ) {
                        let id = found.id.clone();
                        self.select_buffer(id);
                    } else {
                        self.pending_buffer_switch = Some(target);
                    }
                }
            }
            BackendEvent::BufferClosed { buffer_id } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                self.buffers.retain(|b| b.id != full_id);
                self.rebuild_buffer_idx();
                if self.selected_buffer_id.as_deref() == Some(&full_id) {
                    self.selected_buffer_id = self
                        .buffers
                        .iter()
                        .find(|buffer| !buffer.is_matrix_thread())
                        .map(|buffer| buffer.id.clone());
                }
            }
            BackendEvent::BuffersLoaded(bufs) => {
                // Prefix all buffer ids from this connection, then replace those buffers
                let pfx = format!("{}/", conn_prefix);
                self.buffers.retain(|b| !b.id.starts_with(&pfx));
                for mut buf in bufs {
                    buf.id = format!("{}/{}", conn_prefix, buf.id);
                    buf.full_name = format!("{}/{}", conn_prefix, buf.full_name);
                    self.buffers.push(buf);
                }
                self.rebuild_buffer_idx();
                if self.selected_buffer_id.is_none() {
                    if let Some(id) = preferred_chat_buffer_id(
                        &self.buffers,
                        self.last_chat_buffer_name.as_deref(),
                    ) {
                        self.select_buffer(id);
                    }
                }
            }
            BackendEvent::LineAdded { buffer_id, line } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                let is_selected = self.selected_buffer_id.as_deref() == Some(full_id.as_str());
                if line.highlight {
                    log::info!(
                        "LineAdded highlight=true buffer={} selected={} displayed={}",
                        full_id, is_selected, line.displayed
                    );
                }
                let mut should_notify = false;
                let mut buf_name = String::new();
                let mut notify_prefix = String::new();
                let mut notify_message = String::new();
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    if !is_selected && !buf.muted && line.displayed {
                        if line.highlight {
                            buf.activity = crate::relay::models::BufferActivity::Highlight;
                            should_notify = true;
                            buf_name = buf.name.clone();
                            notify_prefix = line.prefix.clone();
                            notify_message = line.message.clone();
                        } else if buf.activity != crate::relay::models::BufferActivity::Highlight {
                            buf.activity = crate::relay::models::BufferActivity::Message;
                        }
                        buf.unread_count = buf.unread_count.saturating_add(1);
                    }
                    buf.messages.push_back(line);
                    if buf.messages.len() > MAX_STORED_LINES {
                        buf.messages.pop_front();
                    }
                }
                if should_notify {
                    self.notify_highlight(&full_id, &buf_name, &notify_prefix, &notify_message);
                }
            }
            BackendEvent::NicklistLoaded { buffer_id, mut nicks } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                nicks.sort_by(|a, b| {
                    fn rank(p: &str) -> u8 {
                        if p.contains('~') { 0 }
                        else if p.contains('&') { 1 }
                        else if p.contains('@') { 2 }
                        else if p.contains('%') { 3 }
                        else if p.contains('+') { 4 }
                        else { 5 }
                    }
                    rank(&a.prefix).cmp(&rank(&b.prefix))
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    buf.nicks = nicks;
                }
            }
            BackendEvent::NickAdded { buffer_id, nick } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    if !buf.nicks.iter().any(|n| n.name == nick.name) {
                        buf.nicks.push(nick);
                    }
                }
            }
            BackendEvent::NickRemoved { buffer_id, nick_name } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    buf.nicks.retain(|n| !n.name.eq_ignore_ascii_case(&nick_name));
                }
            }
            BackendEvent::NickAwayChanged { buffer_id, nick_name, away } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    if let Some(nick) = buf.nicks.iter_mut().find(|n| n.name.eq_ignore_ascii_case(&nick_name)) {
                        nick.away = away;
                    }
                }
            }
            BackendEvent::TopicChanged { buffer_id, topic } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    buf.topic = topic;
                }
            }
            BackendEvent::ActivityChanged { buffer_id, activity, unread_count, markread_ts } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    buf.activity = activity;
                    buf.unread_count = unread_count;
                    if let Some(ts) = markread_ts {
                        buf.last_markread_ts = Some(ts);
                    }
                }
            }
            BackendEvent::LinesLoaded { buffer_id, lines, is_prepend } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                let is_selected = self.selected_buffer_id.as_deref() == Some(full_id.as_str());
                let is_load_more = self.loading_more_buffer_id.as_deref() == Some(full_id.as_str());
                let received_count = lines.len();
                let mut inserted_count = 0;
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    // Update activity for on-connect chathistory replay, but not for
                    // user-triggered "load more" requests (those are explicitly old history).
                    // For prepended history, only count lines newer than the soju read marker.
                    if !is_selected && !buf.muted && !is_load_more {
                        let read_cutoff = if is_prepend { buf.last_markread_ts } else { None };
                        for line in &lines {
                            if !line.displayed { continue; }
                            if let Some(cutoff) = read_cutoff {
                                if line.timestamp <= cutoff { continue; }
                            }
                            if line.highlight {
                                buf.activity = crate::relay::models::BufferActivity::Highlight;
                                buf.unread_count = buf.unread_count.saturating_add(1);
                            } else if buf.activity != crate::relay::models::BufferActivity::Highlight {
                                buf.activity = crate::relay::models::BufferActivity::Message;
                                buf.unread_count = buf.unread_count.saturating_add(1);
                            }
                        }
                    }
                    if is_prepend {
                        let existing_ids: std::collections::HashSet<_> =
                            buf.messages.iter().map(|line| line.id.clone()).collect();
                        let unique_lines: Vec<_> = lines
                            .into_iter()
                            .filter(|line| !existing_ids.contains(&line.id))
                            .collect();
                        inserted_count = unique_lines.len();
                        for line in unique_lines.into_iter().rev() {
                            buf.messages.push_front(line);
                        }
                        while buf.messages.len() > MAX_STORED_LINES {
                            buf.messages.pop_back();
                        }
                    } else {
                        for line in lines {
                            buf.messages.push_back(line);
                        }
                        while buf.messages.len() > MAX_STORED_LINES {
                            buf.messages.pop_front();
                        }
                    }
                }
                if is_load_more {
                    self.loading_more_buffer_id = None;
                    if received_count < LOAD_MORE_LINES || inserted_count == 0 {
                        self.history_exhausted_buffer_ids.insert(full_id);
                    }
                }
            }
            BackendEvent::BufferHidden { buffer_id, hidden } => {
                let full_id = format!("{}/{}", conn_prefix, buffer_id);
                if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                    buf.hidden = hidden;
                }
            }
        }
    }

    pub(crate) fn process_response(&mut self, conn_prefix: &str, resp: WeeChatResponse) {
        if let Some(id) = &resp.request_id {
            if let Some(sequence) = id
                .strip_prefix("_completion:")
                .and_then(|value| value.parse::<u64>().ok())
            {
                self.handle_command_completion(conn_prefix, sequence, resp);
                return;
            } else if id == "_list_buffers" {
                self.handle_buffer_list(conn_prefix, resp);
                return;
            } else if id == "_hotlist" {
                self.handle_hotlist(conn_prefix, resp);
                return;
            } else if id.starts_with("_buffer_lines:") {
                let request = &id[14..];
                let (buffer_id, requested_count) = request
                    .rsplit_once(':')
                    .and_then(|(buffer_id, count)| {
                        count.parse::<usize>().ok().map(|count| (buffer_id, count))
                    })
                    .map(|(buffer_id, count)| (buffer_id.to_owned(), Some(count)))
                    .unwrap_or_else(|| (request.to_owned(), None));
                self.handle_buffer_lines(conn_prefix, &buffer_id, requested_count, resp);
                return;
            } else if id.starts_with("_nicks:") {
                let buffer_id = id[7..].to_string();
                self.handle_nick_list(conn_prefix, &buffer_id, resp);
                return;
            } else if id.starts_with("_buffer_info:") {
                let buffer_id = id[13..].to_string();
                self.handle_buffer_info(conn_prefix, &buffer_id, resp);
                return;
            }
        }

        if let Some(event) = &resp.event_name {
            match event.as_str() {
                "buffer_line_added" => self.handle_line_added(conn_prefix, resp),
                "buffer_line_data_changed" => self.handle_line_changed(conn_prefix, resp),
                "buffer_hidden" => {
                    if let Some(raw_id) = resp.buffer_id.map(|i| i.to_string()) {
                        let full_id = format!("{}/{}", conn_prefix, raw_id);
                        if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                            buf.hidden = true;
                        }
                    }
                }
                "buffer_unhidden" => {
                    if let Some(raw_id) = resp.buffer_id.map(|i| i.to_string()) {
                        let full_id = format!("{}/{}", conn_prefix, raw_id);
                        if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                            buf.hidden = false;
                        }
                    }
                }
                "buffer_title_changed" => {
                    if let Some(raw_id) = resp.buffer_id.map(|i| i.to_string()) {
                        if let Some(conn) = self.connections.iter().find(|c| c.prefix == conn_prefix) {
                            conn.client.refresh_buffer(&raw_id);
                        }
                    }
                }
                "nicklist_nick_added" => {
                    if let Some(raw_id) = resp.buffer_id.map(|i| i.to_string()) {
                        let full_id = format!("{}/{}", conn_prefix, raw_id);
                        if let Some(nick) = resp.body.as_ref().and_then(|b| b.as_object()).and_then(|o| Self::parse_nick_obj(o)) {
                            if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                                if !buf.nicks.iter().any(|n| n.name == nick.name) {
                                    buf.nicks.push(nick);
                                    buf.nicks.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                                }
                            }
                        }
                    }
                }
                "nicklist_nick_removing" => {
                    if let Some(raw_id) = resp.buffer_id.map(|i| i.to_string()) {
                        let full_id = format!("{}/{}", conn_prefix, raw_id);
                        if let Some(name) = resp.body.as_ref().and_then(|b| b.get("name")).and_then(|v| v.as_str()) {
                            if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                                buf.nicks.retain(|n| n.name != name);
                            }
                        }
                    }
                }
                "nicklist_nick_changed" => {
                    if let Some(raw_id) = resp.buffer_id.map(|i| i.to_string()) {
                        let full_id = format!("{}/{}", conn_prefix, raw_id);
                        if let Some(updated) = resp.body.as_ref().and_then(|b| b.as_object()).and_then(|o| Self::parse_nick_obj(o)) {
                            if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                                if let Some(existing) = buf.nicks.iter_mut().find(|n| n.name == updated.name) {
                                    *existing = updated;
                                } else {
                                    buf.nicks.push(updated);
                                    buf.nicks.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                                }
                            }
                        }
                    }
                }
                "buffer_cleared" => {
                    if let Some(raw_id) = resp.buffer_id.map(|i| i.to_string()) {
                        let full_id = format!("{}/{}", conn_prefix, raw_id);
                        if let Some(buf) = self.buffer_by_id_mut(&full_id) {
                            buf.messages.clear();
                            buf.last_read_id = None;
                            buf.visit_start_marker_id = None;
                        }
                    }
                }
                "upgrade" => {
                    self.log_conn_for(conn_prefix, "WeeChat is upgrading — waiting for reload…");
                }
                "upgrade_ended" => {
                    if let Some(conn) = self.connections.iter().find(|c| c.prefix == conn_prefix) {
                        conn.client.fetch_buffer_list();
                    }
                    self.log_conn_for(conn_prefix, "WeeChat upgrade complete — re-synced");
                }
                "buffer_opened" | "buffer_closed" | "buffer_renamed"
                | "buffer_localvar_added" | "buffer_localvar_changed"
                | "buffer_localvar_removed" => {
                    if let Some(conn) = self.connections.iter().find(|c| c.prefix == conn_prefix) {
                        conn.client.fetch_buffer_list();
                    }
                }
                "buffer_hotlist_added" | "buffer_hotlist_updated" => {
                    // New unread activity pushed while connected — remove from cleared set
                    // so the real unread state is applied rather than being suppressed.
                    if let Some(raw_id) = resp.buffer_id.map(|i| i.to_string()) {
                        let full_id = format!("{}/{}", conn_prefix, raw_id);
                        self.cleared_buffer_ids.remove(&full_id);
                    }
                    self.handle_hotlist(conn_prefix, resp);
                }
                "buffer_hotlist_removed" => {
                    if let Some(conn) = self.connections.iter().find(|c| c.prefix == conn_prefix) {
                        conn.client.fetch_hotlist();
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_command_completion(
        &mut self,
        conn_prefix: &str,
        sequence: u64,
        resp: WeeChatResponse,
    ) {
        let Some(pending) = self.command_completion_pending.as_ref() else {
            return;
        };
        if pending.sequence != sequence {
            return;
        }
        let pending = self.command_completion_pending.take().unwrap();
        let expected_prefix = format!("{conn_prefix}/");
        if !pending.buffer_id.starts_with(&expected_prefix)
            || self.selected_buffer_id.as_deref() != Some(pending.buffer_id.as_str())
            || self.input_text != pending.input
            || resp.code != Some(200)
            || resp.body_type.as_deref() != Some("completion")
        {
            self.command_completion = None;
            return;
        }

        let Some(body) = resp.body.as_ref().and_then(Value::as_object) else {
            self.command_completion = None;
            return;
        };
        let context = body
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("null")
            .to_owned();
        let base_word = body
            .get("base_word")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(position_replace) = body
            .get("position_replace")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
        else {
            self.command_completion = None;
            return;
        };
        let add_space = body
            .get("add_space")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let matches = body
            .get("list")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if matches.is_empty() {
            self.command_completion = None;
            return;
        }
        self.command_completion = Some(CommandCompletionState {
            context,
            source_text: pending.input,
            cursor_byte_idx: pending.cursor_byte_idx,
            base_word,
            position_replace,
            add_space,
            matches,
            index: 0,
        });
    }

    fn has_tag(obj: &serde_json::Map<String, Value>, tag: &str) -> bool {
        match obj.get("tags") {
            Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(tag)),
            Some(Value::String(s)) => s.split(',').any(|t| t.trim() == tag),
            _ => false,
        }
    }

    fn tag_value(obj: &serde_json::Map<String, Value>, prefix: &str) -> Option<String> {
        let tags: Vec<&str> = match obj.get("tags") {
            Some(Value::Array(tags)) => tags.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(tags)) => tags.split(',').map(str::trim).collect(),
            _ => Vec::new(),
        };

        tags.into_iter().find_map(|tag| {
            let value = tag.strip_prefix(prefix)?;
            if value.starts_with('$') && !value.chars().any(char::is_whitespace) {
                Some(value.to_string())
            } else {
                None
            }
        })
    }

    fn raw_tag_value(
        obj: &serde_json::Map<String, Value>,
        prefix: &str,
    ) -> Option<String> {
        match obj.get("tags") {
            Some(Value::Array(tags)) => tags
                .iter()
                .filter_map(Value::as_str)
                .find_map(|tag| tag.strip_prefix(prefix).map(str::to_owned)),
            Some(Value::String(tags)) => tags
                .split(',')
                .map(str::trim)
                .find_map(|tag| tag.strip_prefix(prefix).map(str::to_owned)),
            _ => None,
        }
    }

    fn decode_hex_utf8(encoded: &str) -> Option<String> {
        if encoded.len() % 2 != 0 {
            return None;
        }

        let bytes = encoded
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let high = (pair[0] as char).to_digit(16)?;
                let low = (pair[1] as char).to_digit(16)?;
                Some(((high << 4) | low) as u8)
            })
            .collect::<Option<Vec<_>>>()?;

        String::from_utf8(bytes).ok()
    }

    fn matrix_reply_from_tags(
        obj: &serde_json::Map<String, Value>,
    ) -> Option<crate::relay::models::MatrixReplyContext> {
        use crate::relay::models::{
            MatrixReplyContext, MatrixReplyLineKind,
        };

        let kind = if Self::has_tag(obj, "matrix_reply_header") {
            MatrixReplyLineKind::Header
        } else if Self::has_tag(obj, "matrix_reply_quote") {
            MatrixReplyLineKind::Quote
        } else {
            return None;
        };
        let sender = Self::raw_tag_value(obj, "matrix_reply_sender_hex_")
            .and_then(|value| Self::decode_hex_utf8(&value));
        let event_id = Self::raw_tag_value(obj, "matrix_reply_id_")
            .filter(|value| {
                value.starts_with('$')
                    && !value.chars().any(char::is_whitespace)
            });

        Some(MatrixReplyContext {
            event_id,
            sender,
            kind,
        })
    }

    fn decode_media_tag(
        obj: &serde_json::Map<String, Value>,
        prefix: &str,
    ) -> Option<String> {
        let encoded = Self::raw_tag_value(obj, prefix)?;
        String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()
    }

    fn matrix_media_from_tags(
        obj: &serde_json::Map<String, Value>,
    ) -> Option<MatrixMedia> {
        if !Self::has_tag(obj, "matrix_media") {
            return None;
        }

        let mxc_uri = Self::decode_media_tag(obj, "matrix_media_uri_")?;
        let parsed = url::Url::parse(&mxc_uri).ok()?;
        if parsed.scheme() != "mxc"
            || parsed.host_str().is_none()
            || parsed.path().trim_matches('/').is_empty()
        {
            return None;
        }

        let kind = Self::raw_tag_value(obj, "matrix_media_kind_")?;
        if !matches!(kind.as_str(), "audio" | "file" | "image" | "video") {
            return None;
        }

        let name = Self::decode_media_tag(obj, "matrix_media_name_")
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "attachment".to_owned());

        Some(MatrixMedia {
            mxc_uri,
            name,
            kind,
        })
    }

    fn parse_id(v: &Value) -> Option<String> {
        v.as_i64().map(|i| i.to_string())
            .or_else(|| v.as_f64().map(|f| (f as i64).to_string()))
            .or_else(|| v.as_str().map(|s| s.to_string()))
    }

    fn body_as_vec(resp: &WeeChatResponse) -> Vec<&Value> {
        match &resp.body {
            Some(Value::Array(a)) => a.iter().collect(),
            Some(obj) => vec![obj],
            None => vec![],
        }
    }

    fn parse_date(val: Option<&Value>) -> DateTime<Utc> {
        if let Some(s) = val.and_then(|v| v.as_str()) {
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return dt.with_timezone(&Utc);
            }
            if let Ok(secs) = s.parse::<i64>() {
                if let Some(dt) = DateTime::from_timestamp(secs, 0) {
                    return dt;
                }
            }
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
                return chrono::TimeZone::from_local_datetime(&Local, &dt)
                    .single()
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now);
            }
        }
        Utc::now()
    }

    fn extract_metadata(
        obj: &serde_json::Map<String, Value>,
        topic: &mut String,
        modes: &mut String,
        kind: &mut String,
        server: &mut String,
        full_name: &str,
        plugin: &str,
    ) {
        *kind = "unknown".to_string();
        *server = "orphans".to_string();

        if full_name == "weechat" || plugin == "core" || full_name == "core.weechat"
            || full_name.ends_with(".core.weechat")
        {
            *kind = "core".to_string();
            *server = "!00_core".to_string();
            return;
        }

        if let Some(vars) = obj.get("local_variables").and_then(|v| v.as_object()) {
            if let Some(t) = vars.get("topic").and_then(|v| v.as_str()) { *topic = t.to_string(); }
            if let Some(m) = vars.get("modes").and_then(|v| v.as_str()) { *modes = m.to_string(); }
            if let Some(k) = vars.get("type").and_then(|v| v.as_str()) { *kind = k.to_string(); }
            if let Some(s) = vars.get("server").and_then(|v| v.as_str()) { *server = s.to_string().to_lowercase(); }
        }

        if topic.is_empty() {
            if let Some(t) = obj.get("title").and_then(|v| v.as_str()) { *topic = t.to_string(); }
            else if let Some(t) = obj.get("topic").and_then(|v| v.as_str()) { *topic = t.to_string(); }
            else if let Some(t) = obj.get("topic_string").and_then(|v| v.as_str()) { *topic = t.to_string(); }
        }

        if plugin == "irc" {
            let parts: Vec<&str> = full_name.split('.').collect();
            if parts.len() >= 2 {
                let net_candidate = if parts[1] == "server" && parts.len() >= 3 {
                    if *kind == "unknown" { *kind = "server".to_string(); }
                    parts[2]
                } else {
                    parts[1]
                };
                if *server == "orphans" {
                    *server = net_candidate.to_string().to_lowercase();
                }
            }
            if *kind == "unknown" {
                if parts.len() <= 2 || (parts.len() == 3 && parts[1] == "server") { *kind = "server".to_string(); }
                else { *kind = "channel".to_string(); }
            }
        } else if !plugin.is_empty() {
             if *server == "orphans" { *server = plugin.to_lowercase(); }
             if *kind == "unknown" { *kind = "server".to_string(); }
        }
    }

    fn extract_matrix_buffer_metadata(
        obj: &serde_json::Map<String, Value>,
    ) -> (Option<String>, Option<String>) {
        let vars = obj.get("local_variables").and_then(|value| value.as_object());
        let room_id = vars
            .and_then(|vars| vars.get("room_id"))
            .and_then(|value| value.as_str())
            .filter(|value| value.starts_with('!') && !value.chars().any(char::is_whitespace))
            .map(ToOwned::to_owned);
        let thread_root = vars
            .and_then(|vars| vars.get("thread_root"))
            .and_then(|value| value.as_str())
            .filter(|value| value.starts_with('$') && !value.chars().any(char::is_whitespace))
            .map(ToOwned::to_owned);
        (room_id, thread_root)
    }

    fn extract_matrix_member_profiles(
        obj: &serde_json::Map<String, Value>,
    ) -> Option<Vec<MatrixMemberProfile>> {
        let encoded = obj
            .get("local_variables")?
            .as_object()?
            .get("matrix_members_v1")?
            .as_str()?;
        let mut profiles: Vec<MatrixMemberProfile> = serde_json::from_str(encoded).ok()?;
        profiles.retain(|profile| {
            profile.user_id.starts_with('@')
                && !profile.user_id.chars().any(char::is_whitespace)
                && profile.avatar_mxc.as_ref().is_none_or(|uri| {
                    uri.starts_with("mxc://") && !uri.chars().any(char::is_whitespace)
                })
        });
        Some(profiles)
    }

    fn extract_buffer_plugin(obj: &serde_json::Map<String, Value>) -> String {
        obj.get("plugin")
            .and_then(|value| value.as_str())
            .or_else(|| {
                obj.get("local_variables")
                    .and_then(|value| value.as_object())
                    .and_then(|vars| vars.get("plugin"))
                    .and_then(|value| value.as_str())
            })
            .unwrap_or_default()
            .to_owned()
    }

    fn sort_buffers(buffers: &mut Vec<Buffer>) {
        buffers.sort_by(|a, b| {
            if a.server != b.server {
                return a.server.cmp(&b.server);
            }

            let a_is_root = a.kind == "core" || a.kind == "server";
            let b_is_root = b.kind == "core" || b.kind == "server";

            if a_is_root && !b_is_root { return std::cmp::Ordering::Less; }
            if b_is_root && !a_is_root { return std::cmp::Ordering::Greater; }

            a.number.cmp(&b.number)
        });
    }

    fn handle_buffer_list(&mut self, conn_prefix: &str, resp: WeeChatResponse) {
        let body = Self::body_as_vec(&resp);
        let mut new_conn_buffers = Vec::new();

        for val in body {
            if let Some(obj) = val.as_object() {
                let raw_id = obj.get("id").and_then(|v| Self::parse_id(v));

                let number = obj.get("number").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let name = obj.get("short_name").and_then(|v| v.as_str())
                    .or_else(|| obj.get("name").and_then(|v| v.as_str()))
                    .unwrap_or("unknown").to_string();
                let raw_full_name = obj.get("name").and_then(|v| v.as_str()).unwrap_or(&name).to_string();
                let plugin = Self::extract_buffer_plugin(obj);
                let hidden = obj.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false);
                let has_nicklist = obj.get("nicklist").and_then(|v| v.as_bool()).unwrap_or(true);
                let relay_last_read_id = obj.get("last_read_line_id").and_then(|v| Self::parse_id(v))
                    .or_else(|| obj.get("last_read_line").and_then(|v| v.as_object()).and_then(|o| o.get("id")).and_then(|v| Self::parse_id(v)));

                if let Some(raw_id) = raw_id {
                    let full_id = format!("{}/{}", conn_prefix, raw_id);
                    let full_full_name = format!("{}/{}", conn_prefix, raw_full_name);
                    let saved_read_marker = self.read_markers.get(&full_full_name);

                    let mut topic = String::new();
                    let mut modes = String::new();
                    let mut kind = String::new();
                    let mut server = String::new();
                    let own_nick = obj.get("local_variables")
                        .and_then(|value| value.as_object())
                        .and_then(|vars| vars.get("nick"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_owned();

                    let mut messages = std::collections::VecDeque::new();
                    let mut nicks = Vec::new();
                    let mut mention_candidates = Vec::new();
                    let mut matrix_member_profiles = Vec::new();
                    let mut activity = BufferActivity::None;
                    let mut unread_count = 0u32;
                    let mut last_read_id = None;
                    let mut visit_start_marker_id = None;
                    let (matrix_room_id, matrix_thread_root) =
                        Self::extract_matrix_buffer_metadata(obj);

                    if let Some(existing) = self.buffer_by_id(&full_id) {
                        messages = existing.messages.clone();
                        nicks = existing.nicks.clone();
                        mention_candidates = existing.mention_candidates.clone();
                        matrix_member_profiles = existing.matrix_member_profiles.clone();
                        activity = existing.activity;
                        unread_count = existing.unread_count;
                        last_read_id = existing.last_read_id.clone();
                        visit_start_marker_id = existing.visit_start_marker_id.clone();
                    }
                    let muted = self.muted_buffer_names.contains(&full_full_name);
                    if relay_last_read_id.is_some() {
                        last_read_id = relay_last_read_id.clone();
                    } else if let Some(marker) = saved_read_marker {
                        last_read_id = Some(marker.line_id.clone());
                    }

                    // Use raw_full_name (without prefix) for metadata extraction
                    Self::extract_metadata(obj, &mut topic, &mut modes, &mut kind, &mut server, &raw_full_name, &plugin);
                    if let Some(encoded) = obj.get("local_variables")
                        .and_then(|value| value.as_object())
                        .and_then(|vars| vars.get("matrix_mentions"))
                        .and_then(|value| value.as_str())
                    {
                        if let Ok(candidates) = serde_json::from_str(encoded) {
                            mention_candidates = candidates;
                        }
                    }
                    if let Some(profiles) = Self::extract_matrix_member_profiles(obj) {
                        matrix_member_profiles = profiles;
                    }

                    // Core and server buffers never have a usable nicklist regardless of
                    // what the relay reports (unwrap_or(true) above can over-report).
                    let effective_nicklist = has_nicklist
                        && kind != "core"
                        && kind != "server";

                    new_conn_buffers.push(Buffer {
                        id: full_id,
                        number,
                        name,
                        full_name: full_full_name,
                        plugin,
                        kind,
                        server,
                        own_nick,
                        messages,
                        nicks,
                        mention_candidates,
                        matrix_member_profiles,
                        activity,
                        unread_count,
                        last_read_id,
                        topic,
                        modes,
                        hidden,
                        muted,
                        has_nicklist: effective_nicklist,
                        matrix_room_id,
                        matrix_thread_root,
                        visit_start_marker_id,
                        last_markread_ts: None,
                    });
                }
            }
        }

        if !new_conn_buffers.is_empty() {
            let network_count = {
                let mut seen = std::collections::HashSet::new();
                new_conn_buffers.iter().filter(|b| b.kind != "core").for_each(|b| { seen.insert(&b.server); });
                seen.len()
            };
            self.log_conn_for(conn_prefix, format!(
                "← GET /api/buffers  {} buffers, {} network(s)",
                new_conn_buffers.len(), network_count
            ));
            Self::sort_buffers(&mut new_conn_buffers);

            // Remove old buffers for this connection, then add new ones
            let pfx = format!("{}/", conn_prefix);
            self.buffers.retain(|b| !b.id.starts_with(&pfx));
            self.buffers.extend(new_conn_buffers);

            // Re-apply the user's custom ordering only while it preserves
            // connection/server group boundaries. Legacy relay IDs without a
            // connection prefix otherwise sort all roots before all children.
            if !self.buffer_order.is_empty() {
                if !apply_saved_buffer_order(&mut self.buffers, &self.buffer_order) {
                    self.buffer_order = self
                        .buffers
                        .iter()
                        .filter(|buffer| !buffer.is_matrix_thread())
                        .map(|buffer| buffer.id.clone())
                        .collect();
                }
            }

            // When multiple connections are present, group all buffers by connection prefix
            // so each connection's buffers appear together in the sidebar.
            let conn_count = {
                let mut seen = std::collections::HashSet::new();
                for b in &self.buffers {
                    if let Some(p) = b.id.split('/').next() { seen.insert(p.to_string()); }
                }
                seen.len()
            };
            if conn_count > 1 {
                self.buffers.sort_by(|a, b| {
                    let pa = a.id.split('/').next().unwrap_or("");
                    let pb = b.id.split('/').next().unwrap_or("");
                    pa.cmp(pb)
                });
            }
            self.rebuild_buffer_idx();

            let parent_room_id = self
                .selected_buffer_id
                .as_ref()
                .and_then(|selected_id| {
                    self.buffer_by_id(selected_id)
                        .filter(|buffer| buffer.is_matrix_thread())
                        .and_then(|buffer| buffer.matrix_room_id.clone())
                });
            if let Some(parent_room_id) = parent_room_id {
                self.selected_buffer_id = self
                    .buffers
                    .iter()
                    .find(|buffer| {
                        !buffer.is_matrix_thread()
                            && buffer.matrix_room_id.as_deref()
                                == Some(parent_room_id.as_str())
                    })
                    .map(|buffer| buffer.id.clone());
            }

            if let Some(target) = self.pending_buffer_switch.take() {
                if let Some(found) = self.buffers.iter().find(|b| b.name == target || b.full_name.ends_with(&target)) {
                    let id = found.id.clone();
                    self.select_buffer(id);
                } else {
                    self.pending_buffer_switch = Some(target);
                }
            }

            if self.selected_buffer_id.is_none() {
                let connection_buffers = self
                    .buffers
                    .iter()
                    .filter(|buffer| buffer.id.starts_with(&format!("{conn_prefix}/")))
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some(buffer_id) = preferred_chat_buffer_id(
                    &connection_buffers,
                    self.last_chat_buffer_name.as_deref(),
                ) {
                    self.select_buffer(buffer_id);
                }
            }

            self.log_conn_for(conn_prefix, "→ GET /api/hotlist");
            if let Some(conn) = self.connections.iter().find(|c| c.prefix == conn_prefix) {
                conn.client.fetch_hotlist();
            }
        }
    }

    fn handle_hotlist(&mut self, conn_prefix: &str, resp: WeeChatResponse) {
        let body = Self::body_as_vec(&resp);
        let is_initial = resp.request_id.as_deref() == Some("_hotlist");
        let entry_count = body.len();
        if is_initial {
            self.log_conn_for(conn_prefix, format!("← GET /api/hotlist  {} active entr{}", entry_count, if entry_count == 1 { "y" } else { "ies" }));
        }
        for val in body {
            if let Some(obj) = val.as_object() {
                let raw_buffer_id = obj.get("buffer_id").and_then(|v| Self::parse_id(v));
                let priority = obj.get("priority")
                    .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                    .unwrap_or(0);

                if let Some(raw_buffer_id) = raw_buffer_id {
                    let buffer_id = format!("{}/{}", conn_prefix, raw_buffer_id);
                    if self.selected_buffer_id.as_deref() == Some(&buffer_id) {
                        continue;
                    }
                    if self.buffers.iter().any(|b| b.id == buffer_id && b.muted) {
                        continue;
                    }
                    if self.cleared_buffer_ids.contains(&buffer_id) {
                        continue;
                    }
                    if let Some(buffer) = self.buffer_by_id_mut(&buffer_id) {
                        buffer.activity = match priority {
                            3 => BufferActivity::Highlight,
                            2 | 1 => BufferActivity::Message,
                            _ => BufferActivity::Metadata,
                        };
                        if let Some(count_arr) = obj.get("count").and_then(|v| v.as_array()) {
                            let msg  = count_arr.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
                            let priv_msg = count_arr.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
                            let hl   = count_arr.get(3).and_then(|v| v.as_i64()).unwrap_or(0);
                            buffer.unread_count = (msg + priv_msg + hl) as u32;
                        }
                    }
                }
            }
        }
    }

    fn handle_buffer_lines(
        &mut self,
        conn_prefix: &str,
        raw_buffer_id: &str,
        requested_count: Option<usize>,
        resp: WeeChatResponse,
    ) {
        let full_buffer_id = format!("{}/{}", conn_prefix, raw_buffer_id);
        let is_load_more = self.loading_more_buffer_id.as_deref() == Some(&full_buffer_id);
        if resp.code.is_some_and(|code| !(200..300).contains(&code)) {
            if is_load_more {
                self.loading_more_buffer_id = None;
            }
            self.log_conn_for(
                conn_prefix,
                format!(
                    "← GET /api/buffers/{}/lines failed: {}",
                    raw_buffer_id,
                    resp.message.unwrap_or_else(|| "unknown relay error".to_owned())
                ),
            );
            return;
        }
        let body = Self::body_as_vec(&resp);
        let mut lines: Vec<Line> = body.iter().filter_map(|val| {
            let obj = val.as_object()?;
            if Self::has_tag(obj, "matrix_history_page") {
                return None;
            }
            let displayed = obj.get("displayed").and_then(|v| v.as_bool()).unwrap_or(true);
            let id = obj.get("id").and_then(|v| Self::parse_id(v))
                .unwrap_or_else(|| "unknown".to_string());
            let prefix = obj.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let message = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let timestamp = Self::parse_date(obj.get("date"));
            let highlight = obj.get("highlight").and_then(|v| v.as_bool()).unwrap_or(false);
            let mut line = Line::new(
                id,
                timestamp,
                prefix.to_string(),
                message.to_string(),
                displayed,
                highlight,
            );
            line.matrix_event_id = Self::tag_value(obj, "matrix_id_");
            line.matrix_reply = Self::matrix_reply_from_tags(obj);
            line.matrix_media = Self::matrix_media_from_tags(obj);
            Some(line)
        }).collect();
        sort_lines_chronologically(&mut lines);

        let mut log_entry: Option<String> = None;
        let is_selected = self.selected_buffer_id.as_deref() == Some(&full_buffer_id);
        let mut history_exhausted = false;
        let mut marker_to_save = None;
        if let Some(idx) = self.buffer_idx_of(&full_buffer_id) {
            let full_name = self.buffers[idx].full_name.clone();
            let saved_read_marker = self.read_markers.get(&full_name).cloned();
            let buffer = &mut self.buffers[idx];
            let mut deque: std::collections::VecDeque<Line> = lines.into();
            if deque.len() > MAX_STORED_LINES {
                let excess = deque.len() - MAX_STORED_LINES;
                deque.drain(0..excess);
            }
            let line_count = deque.len();
            history_exhausted = is_load_more
                && buffer.plugin != "matrix"
                && requested_count
                    .is_some_and(|requested| history_snapshot_is_exhausted(line_count, requested));
            log_entry = Some(format!(
                "← GET /api/buffers/{}/lines  {} lines{}  [#{}]",
                raw_buffer_id, line_count,
                if is_load_more { " (load more)" } else { "" },
                buffer.name
            ));
            buffer.messages = deque;
            if is_selected {
                if let Some(marker) = &saved_read_marker {
                    let visit_marker_missing = buffer
                        .visit_start_marker_id
                        .as_ref()
                        .is_none_or(|id| !buffer.messages.iter().any(|line| line.id == *id));
                    if visit_marker_missing {
                        buffer.visit_start_marker_id =
                            marker.restore_visit_line_id(&buffer.messages);
                    }
                }
                if let Some(last) = buffer.messages.back() {
                    buffer.last_read_id = Some(last.id.clone());
                    marker_to_save = Some((full_name, SavedReadMarker::from_line(last)));
                }
            } else if let Some(marker) = saved_read_marker {
                buffer.last_read_id = marker.restore_line_id(&buffer.messages);
            } else if buffer.last_read_id.is_none() {
                if let Some(last) = buffer.messages.back() {
                    buffer.last_read_id = Some(last.id.clone());
                }
            }
        }
        if let Some((full_name, marker)) = marker_to_save {
            self.read_markers.insert(full_name, marker);
        }
        if is_load_more {
            self.loading_more_buffer_id = None;
        }
        if history_exhausted {
            self.history_exhausted_buffer_ids.insert(full_buffer_id);
        }
        if let Some(entry) = log_entry {
            self.log_conn_for(conn_prefix, entry);
        }
    }

    fn handle_buffer_info(&mut self, conn_prefix: &str, raw_buffer_id: &str, resp: WeeChatResponse) {
        let full_buffer_id = format!("{}/{}", conn_prefix, raw_buffer_id);
        let body = Self::body_as_vec(&resp);
        for val in body {
            if let Some(obj) = val.as_object() {
                let refreshed_plugin = Self::extract_buffer_plugin(obj);
                let (matrix_room_id, matrix_thread_root) =
                    Self::extract_matrix_buffer_metadata(obj);
                if let Some(buffer) = self.buffer_by_id_mut(&full_buffer_id) {
                    // Strip prefix from full_name for metadata extraction
                    let pfx = format!("{}/", conn_prefix);
                    let raw_full_name = buffer.full_name.strip_prefix(&pfx).unwrap_or(&buffer.full_name).to_string();
                    if !refreshed_plugin.is_empty() {
                        buffer.plugin = refreshed_plugin.clone();
                    }
                    let plugin = buffer.plugin.clone();
                    Self::extract_metadata(obj, &mut buffer.topic, &mut buffer.modes, &mut buffer.kind, &mut buffer.server, &raw_full_name, &plugin);
                    if let Some(own_nick) = obj.get("local_variables")
                        .and_then(|value| value.as_object())
                        .and_then(|vars| vars.get("nick"))
                        .and_then(|value| value.as_str())
                    {
                        buffer.own_nick = own_nick.to_owned();
                    }
                    buffer.matrix_room_id = matrix_room_id;
                    buffer.matrix_thread_root = matrix_thread_root;
                    if let Some(encoded) = obj.get("local_variables")
                        .and_then(|value| value.as_object())
                        .and_then(|vars| vars.get("matrix_mentions"))
                        .and_then(|value| value.as_str())
                    {
                        if let Ok(candidates) = serde_json::from_str(encoded) {
                            buffer.mention_candidates = candidates;
                        }
                    }
                    if let Some(profiles) = Self::extract_matrix_member_profiles(obj) {
                        buffer.matrix_member_profiles = profiles;
                    }
                }
            }
        }
    }

    fn handle_nick_list(&mut self, conn_prefix: &str, raw_buffer_id: &str, resp: WeeChatResponse) {
        let full_buffer_id = format!("{}/{}", conn_prefix, raw_buffer_id);
        if let Some(body) = &resp.body {
            let mut nicks = Vec::new();
            self.extract_nicks(body, &mut nicks);
            if let Some(buffer) = self.buffer_by_id_mut(&full_buffer_id) {
                buffer.nicks = nicks;
            }
        }
    }

    fn parse_nick_obj(obj: &serde_json::Map<String, Value>) -> Option<Nick> {
        let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() { return None; }
        let prefix = obj.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
        let color = obj.get("color").and_then(|v| v.as_str()).unwrap_or("");
        let color_ansi = if color.is_empty() {
            obj.get("color_name").and_then(|v| v.as_str()).unwrap_or("").to_string()
        } else {
            color.to_string()
        };
        Some(Nick { name: name.to_string(), prefix: prefix.to_string(), color_ansi, away: false })
    }

    fn extract_nicks(&self, val: &Value, nicks: &mut Vec<Nick>) {
        if let Some(obj) = val.as_object() {
            if let Some(Value::Array(nick_arr)) = obj.get("nicks") {
                for n in nick_arr {
                    if let Some(no) = n.as_object() {
                        if let Some(nick) = Self::parse_nick_obj(no) {
                            nicks.push(nick);
                        }
                    }
                }
            }
            if let Some(Value::Array(groups)) = obj.get("groups") {
                for g in groups {
                    self.extract_nicks(g, nicks);
                }
            }
        }
    }

    /// Fire a highlight notification for `buffer_id` (full prefixed id) with a
    /// per-buffer 3-second cooldown. Also requests user-attention so the dock
    /// bounces / taskbar flashes. Used by both the WeeChat and IRC backends.
    /// Title format:
    ///   - channel highlight: `<channel> — <sender>`
    ///   - private message:  `<sender>` (no channel prefix)
    pub(crate) fn notify_highlight(
        &mut self,
        buffer_id: &str,
        buffer_name: &str,
        prefix: &str,
        message: &str,
    ) {
        let now = std::time::Instant::now();
        let cooldown = std::time::Duration::from_secs(3);
        let suppress = self
            .last_notif_at
            .get(buffer_id)
            .map(|last| now.duration_since(*last) < cooldown)
            .unwrap_or(false);
        if suppress {
            return;
        }
        self.last_notif_at.insert(buffer_id.to_string(), now);
        self.request_attention = true;

        let sender = Self::strip_ansi(prefix);
        let body = Self::strip_ansi(message);

        // Channel buffers in IRC start with #/&/!, WeeChat passes the same. If
        // the buffer name looks like a channel, prefix the title with it; for
        // PMs (or core/server buffers) just use the sender.
        let is_channel_like = buffer_name.starts_with('#')
            || buffer_name.starts_with('&')
            || buffer_name.starts_with('!');
        let title = if is_channel_like && !sender.is_empty() {
            format!("{} — {}", buffer_name, sender)
        } else if sender.is_empty() {
            buffer_name.to_string()
        } else {
            sender
        };

        crate::ui::notify::show(crate::ui::notify::Notification {
            app_name: "WeeChatRS".to_string(),
            title,
            body,
        });
    }

    pub(crate) fn strip_ansi(text: &str) -> String {
        ansi_re().replace_all(text, "").to_string()
    }

    fn handle_line_added(&mut self, conn_prefix: &str, resp: WeeChatResponse) {
        let body = Self::body_as_vec(&resp);
        for val in body {
            if let Some(obj) = val.as_object() {
                let displayed = obj.get("displayed").and_then(|v| v.as_bool()).unwrap_or(true);
                let raw_buffer_id = resp.buffer_id.map(|i| i.to_string())
                    .or_else(|| obj.get("buffer_id").and_then(|v| Self::parse_id(v)));

                let is_highlight = obj.get("highlight").and_then(|v| v.as_bool()).unwrap_or(false);
                let notify_level = obj.get("notify_level")
                    .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                    .unwrap_or(0);
                let is_self_msg = Self::has_tag(obj, "self_msg");
                let is_notify_none = Self::has_tag(obj, "notify_none");
                let is_join_part = Self::has_tag(obj, "irc_join")
                    || Self::has_tag(obj, "irc_part")
                    || Self::has_tag(obj, "irc_quit");

                if let Some(raw_buffer_id) = raw_buffer_id {
                    let buffer_id = format!("{}/{}", conn_prefix, raw_buffer_id);
                    let prefix = obj.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
                    let message = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    if Self::has_tag(obj, "matrix_history_page") {
                        if let Some((added, exhausted)) = matrix_history_page_status(message) {
                            if exhausted {
                                self.history_exhausted_buffer_ids.insert(buffer_id.clone());
                            }
                            if added > 0 {
                                let current_len = self
                                    .buffer_by_id(&buffer_id)
                                    .map_or(added, |buffer| buffer.messages.len());
                                let count = (current_len + LOAD_MORE_LINES).min(MAX_STORED_LINES);
                                self.history_request_counts.insert(buffer_id.clone(), count);
                                if let Some((client, raw_id)) = self.client_for_buffer(&buffer_id) {
                                    // Matrix rewrites physical WeeChat lines while sorting the
                                    // newly fetched page. Refresh one authoritative snapshot only
                                    // after its tagged completion marker, so relay clients see the
                                    // final chronological order rather than transient line edits.
                                    client.fetch_lines(&raw_id, count);
                                } else {
                                    self.loading_more_buffer_id = None;
                                }
                            } else if self.loading_more_buffer_id.as_deref() == Some(&buffer_id) {
                                self.loading_more_buffer_id = None;
                            }
                        }
                        continue;
                    }
                    let id = obj.get("id").and_then(|v| Self::parse_id(v))
                        .unwrap_or_else(|| Utc::now().timestamp_nanos_opt().unwrap_or(0).to_string());
                    let timestamp = Self::parse_date(obj.get("date"));

                    let mut line = Line::new(
                        id,
                        timestamp,
                        prefix.to_string(),
                        message.to_string(),
                        displayed,
                        is_highlight,
                    );
                    line.matrix_event_id = Self::tag_value(obj, "matrix_id_");
                    line.matrix_reply = Self::matrix_reply_from_tags(obj);
                    line.matrix_media = Self::matrix_media_from_tags(obj);

                    let is_selected = self.selected_buffer_id.as_deref() == Some(&buffer_id);
                    let mut notify_data: Option<(String, String, String)> = None;
                    if let Some(idx) = self.buffer_idx_of(&buffer_id) {
                        let buffer = &mut self.buffers[idx];
                        if !buffer.messages.iter().any(|m| m.id == line.id) {
                            let is_historical = buffer
                                .messages
                                .back()
                                .is_some_and(|latest| line.timestamp < latest.timestamp);
                            if is_historical {
                                let insert_at = buffer
                                    .messages
                                    .iter()
                                    .position(|existing| existing.timestamp > line.timestamp)
                                    .unwrap_or(buffer.messages.len());
                                buffer.messages.insert(insert_at, line.clone());
                            } else {
                                buffer.messages.push_back(line.clone());
                            }
                            if buffer.messages.len() > MAX_STORED_LINES {
                                buffer.messages.pop_front();
                            }

                            if is_selected {
                                if !is_historical {
                                    buffer.last_read_id = Some(line.id.clone());
                                    self.read_markers.insert(
                                        buffer.full_name.clone(),
                                        SavedReadMarker::from_line(&line),
                                    );
                                }
                            } else if displayed && !buffer.muted && !is_notify_none && !is_self_msg {
                                let activity = if is_highlight || notify_level == 3 {
                                    BufferActivity::Highlight
                                } else if notify_level == 2 {
                                    BufferActivity::Message
                                } else if is_join_part {
                                    BufferActivity::Metadata
                                } else {
                                    BufferActivity::Metadata
                                };

                                if !is_join_part {
                                    buffer.unread_count = buffer.unread_count.saturating_add(1);
                                }

                                if activity > buffer.activity {
                                    buffer.activity = activity;
                                    self.cleared_buffer_ids.remove(&buffer_id);
                                }

                                if (is_highlight || notify_level == 3) && !buffer.muted {
                                    notify_data = Some((
                                        buffer.name.clone(),
                                        prefix.to_string(),
                                        message.to_string(),
                                    ));
                                }
                            }
                        }
                    }
                    if let Some((name, p, m)) = notify_data {
                        self.notify_highlight(&buffer_id, &name, &p, &m);
                    }
                }
            }
        }
    }

    fn handle_line_changed(&mut self, conn_prefix: &str, resp: WeeChatResponse) {
        let body = Self::body_as_vec(&resp);
        for val in body {
            if let Some(obj) = val.as_object() {
                let raw_buffer_id = resp.buffer_id.map(|i| i.to_string())
                    .or_else(|| obj.get("buffer_id").and_then(|v| Self::parse_id(v)));
                let line_id = obj.get("id").and_then(|v| Self::parse_id(v));
                let displayed = obj.get("displayed").and_then(|v| v.as_bool()).unwrap_or(true);

                if let (Some(raw_buffer_id), Some(line_id)) = (raw_buffer_id, line_id) {
                    let buffer_id = format!("{}/{}", conn_prefix, raw_buffer_id);
                    if let Some(buffer) = self.buffer_by_id_mut(&buffer_id) {
                        if let Some(line) = buffer.messages.iter_mut().find(|m| m.id == line_id) {
                            line.displayed = displayed;
                            if let Some(event_id) = Self::tag_value(obj, "matrix_id_") {
                                line.matrix_event_id = Some(event_id);
                            }
                            if let Some(reply) = Self::matrix_reply_from_tags(obj) {
                                line.matrix_reply = Some(reply);
                            }
                        } else if displayed {
                            let prefix = obj.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
                            let message = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");
                            let timestamp = Self::parse_date(obj.get("date"));
                            let mut line = Line::new(
                                line_id,
                                timestamp,
                                prefix.to_string(),
                                message.to_string(),
                                displayed,
                                false,
                            );
                            line.matrix_event_id = Self::tag_value(obj, "matrix_id_");
                            line.matrix_reply = Self::matrix_reply_from_tags(obj);
                            line.matrix_media = Self::matrix_media_from_tags(obj);
                            buffer.messages.push_back(line);
                            if buffer.messages.len() > MAX_STORED_LINES {
                                buffer.messages.pop_front();
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn matrix_member_profiles_require_safe_structured_identity_and_avatar() {
        let object = serde_json::json!({
            "local_variables": {
                "matrix_members_v1": serde_json::to_string(&serde_json::json!([
                    {
                        "user_id": "@ada:example.org",
                        "display_name": "Ada",
                        "nick": "Ada",
                        "membership": "join",
                        "role": "moderator",
                        "power_level": 50,
                        "avatar_mxc": "mxc://example.org/avatar"
                    },
                    {
                        "user_id": "@bad id:example.org",
                        "display_name": "Bad",
                        "nick": "Bad",
                        "membership": "join",
                        "role": "member",
                        "power_level": 0,
                        "avatar_mxc": "https://example.org/avatar.png"
                    }
                ])).unwrap()
            }
        });
        let profiles = super::WeeChatApp::extract_matrix_member_profiles(
            object.as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].user_id, "@ada:example.org");
        assert_eq!(profiles[0].avatar_mxc.as_deref(), Some("mxc://example.org/avatar"));
    }

    use serde_json::json;

    use crate::relay::models::{Buffer, BufferActivity, Line, MatrixReplyLineKind};
    use chrono::{TimeZone, Utc};
    use std::collections::VecDeque;

    use super::{
        apply_saved_buffer_order, buffer_groups_are_valid, matrix_history_page_status,
        sort_lines_chronologically, WeeChatApp,
    };

    fn sidebar_buffer(id: &str, number: i32, server: &str, kind: &str) -> Buffer {
        Buffer {
            id: id.to_owned(),
            number,
            name: id.to_owned(),
            full_name: id.to_owned(),
            plugin: if server == "matrix" { "matrix" } else { "irc" }.to_owned(),
            kind: kind.to_owned(),
            server: server.to_owned(),
            own_nick: String::new(),
            messages: VecDeque::new(),
            nicks: Vec::new(),
            mention_candidates: Vec::new(),
            matrix_member_profiles: Vec::new(),
            activity: BufferActivity::None,
            unread_count: 0,
            last_read_id: None,
            last_markread_ts: None,
            topic: String::new(),
            modes: String::new(),
            hidden: false,
            muted: false,
            has_nicklist: kind != "server",
            matrix_room_id: None,
            matrix_thread_root: None,
            visit_start_marker_id: None,
        }
    }

    #[test]
    fn stale_unprefixed_order_cannot_flatten_server_groups() {
        let mut buffers = vec![
            sidebar_buffer("localhost/libera-root", 1, "libera", "server"),
            sidebar_buffer("localhost/libera-room", 2, "libera", "channel"),
            sidebar_buffer("localhost/matrix-root", 1, "matrix", "server"),
            sidebar_buffer("localhost/matrix-room", 3, "matrix", "channel"),
        ];
        let grouped_ids = buffers
            .iter()
            .map(|buffer| buffer.id.clone())
            .collect::<Vec<_>>();
        let legacy_order = vec![
            "libera-root".to_owned(),
            "matrix-root".to_owned(),
            "libera-room".to_owned(),
            "matrix-room".to_owned(),
        ];

        assert!(!apply_saved_buffer_order(&mut buffers, &legacy_order));
        assert!(buffer_groups_are_valid(&buffers));
        assert_eq!(
            buffers
                .iter()
                .map(|buffer| buffer.id.clone())
                .collect::<Vec<_>>(),
            grouped_ids,
        );
    }

    #[test]
    fn valid_saved_order_can_move_complete_server_groups() {
        let mut buffers = vec![
            sidebar_buffer("localhost/libera-root", 1, "libera", "server"),
            sidebar_buffer("localhost/libera-room", 2, "libera", "channel"),
            sidebar_buffer("localhost/matrix-root", 1, "matrix", "server"),
            sidebar_buffer("localhost/matrix-room", 3, "matrix", "channel"),
        ];
        let order = vec![
            "localhost/matrix-root".to_owned(),
            "localhost/matrix-room".to_owned(),
            "localhost/libera-root".to_owned(),
            "localhost/libera-room".to_owned(),
        ];

        assert!(apply_saved_buffer_order(&mut buffers, &order));
        assert!(buffer_groups_are_valid(&buffers));
        assert_eq!(
            buffers
                .iter()
                .map(|buffer| buffer.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "localhost/matrix-root",
                "localhost/matrix-room",
                "localhost/libera-root",
                "localhost/libera-room",
            ],
        );
    }

    fn timeline_line(id: &str, second: i64) -> Line {
        Line::new(
            id.to_owned(),
            Utc.timestamp_opt(second, 0).single().unwrap(),
            String::new(),
            id.to_owned(),
            true,
            false,
        )
    }

    #[test]
    fn relay_snapshots_are_stably_normalized_oldest_first() {
        let mut lines = vec![
            timeline_line("newest", 300),
            timeline_line("same-time-first", 200),
            timeline_line("same-time-second", 200),
            timeline_line("oldest", 100),
        ];

        sort_lines_chronologically(&mut lines);

        assert_eq!(
            lines.iter().map(|line| line.id.as_str()).collect::<Vec<_>>(),
            vec!["oldest", "same-time-first", "same-time-second", "newest"],
        );
    }

    #[test]
    fn parses_matrix_history_completion_marker() {
        assert_eq!(
            matrix_history_page_status("matrix_history_page added=200 exhausted=0"),
            Some((200, false))
        );
        assert_eq!(
            matrix_history_page_status(
                "matrix_history_page added=0 exhausted=1 state=unavailable"
            ),
            Some((0, true))
        );
        assert_eq!(matrix_history_page_status("ordinary message"), None);
    }

    #[test]
    fn matrix_event_id_is_read_from_weechat_tags() {
        let array = json!({
            "tags": ["notify_message", "matrix_id_$selected:remote.example"]
        });
        let string = json!({
            "tags": "notify_message, matrix_id_$older:remote.example"
        });

        assert_eq!(
            WeeChatApp::tag_value(array.as_object().unwrap(), "matrix_id_"),
            Some("$selected:remote.example".to_owned())
        );
        assert_eq!(
            WeeChatApp::tag_value(string.as_object().unwrap(), "matrix_id_"),
            Some("$older:remote.example".to_owned())
        );
    }

    #[test]
    fn matrix_reply_metadata_is_read_from_weechat_tags() {
        let object = json!({
            "tags": [
                "matrix_reply",
                "matrix_reply_header",
                "matrix_reply_sender_hex_416c69636520f09f988a",
                "matrix_reply_id_$original:remote.example"
            ]
        });
        let reply =
            WeeChatApp::matrix_reply_from_tags(object.as_object().unwrap())
                .expect("reply metadata");

        assert!(matches!(reply.kind, MatrixReplyLineKind::Header));
        assert_eq!(reply.sender.as_deref(), Some("Alice 😊"));
        assert_eq!(
            reply.event_id.as_deref(),
            Some("$original:remote.example")
        );
    }

    #[test]
    fn legacy_matrix_reply_line_stays_plain_text() {
        let object = json!({
            "tags": ["matrix_reply", "matrix_id_$reply:remote.example"]
        });

        assert!(
            WeeChatApp::matrix_reply_from_tags(object.as_object().unwrap())
                .is_none()
        );
    }

    #[test]
    fn matrix_thread_requires_exact_room_and_root_localvars() {
        let object = serde_json::json!({
            "local_variables": {
                "plugin": "matrix",
                "room_id": "!room:example.org",
                "thread_root": "$root:example.org"
            }
        });
        assert_eq!(
            WeeChatApp::extract_matrix_buffer_metadata(object.as_object().unwrap()),
            (
                Some("!room:example.org".to_owned()),
                Some("$root:example.org".to_owned())
            )
        );
        assert_eq!(
            WeeChatApp::extract_buffer_plugin(object.as_object().unwrap()),
            "matrix"
        );

        let malformed = serde_json::json!({
            "local_variables": {
                "room_id": "#not-a-room",
                "thread_root": "$bad root"
            }
        });
        assert_eq!(
            WeeChatApp::extract_matrix_buffer_metadata(malformed.as_object().unwrap()),
            (None, None)
        );
    }

    #[test]
    fn malformed_matrix_event_tag_is_not_replyable() {
        let object = json!({
            "tags": ["matrix_id_not-an-event", "matrix_id_$bad event"]
        });

        assert_eq!(
            WeeChatApp::tag_value(object.as_object().unwrap(), "matrix_id_"),
            None
        );
    }
}
