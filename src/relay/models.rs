use serde::{Deserialize, Serialize};
use serde_json::Value;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use crate::ui::ansi::{ANSIParser, ANSISection};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WeeChatResponse {
    pub request_id: Option<String>,
    pub event_name: Option<String>,
    pub code: Option<i64>,
    pub message: Option<String>,
    pub body_type: Option<String>,
    pub buffer_id: Option<i64>,
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BufferActivity {
    None = 0,
    Metadata = 1,
    Message = 2,
    Highlight = 3,
}

#[derive(Debug, Clone)]
pub struct Buffer {
    pub id: String,
    pub number: i32,
    pub name: String,
    pub full_name: String,
    pub plugin: String,
    pub kind: String,
    pub server: String,
    pub own_nick: String,
    pub messages: VecDeque<Line>,
    pub nicks: Vec<Nick>,
    pub mention_candidates: Vec<MentionCandidate>,
    pub matrix_member_profiles: Vec<MatrixMemberProfile>,
    pub activity: BufferActivity,
    pub unread_count: u32,
    pub last_read_id: Option<String>,
    pub last_markread_ts: Option<DateTime<Utc>>,
    pub topic: String,
    pub modes: String,
    pub hidden: bool,
    pub muted: bool,
    pub has_nicklist: bool,
    pub matrix_room_id: Option<String>,
    pub matrix_predecessor_room_id: Option<String>,
    pub matrix_replacement_room_id: Option<String>,
    pub matrix_thread_root: Option<String>,
    pub matrix_upload_v1: bool,
    pub matrix_avatar_mxc: Option<String>,
    /// Snapshot of last_read_id taken when the buffer was first entered this session.
    /// Used to anchor the unread divider while the user views the buffer.
    pub visit_start_marker_id: Option<String>,
}

impl Buffer {
    pub fn is_matrix_thread(&self) -> bool {
        self.plugin == "matrix"
            && self.matrix_room_id.is_some()
            && self.matrix_thread_root.is_some()
    }

    pub fn own_mention_aliases(&self) -> Vec<String> {
        if self.own_nick.is_empty() {
            return Vec::new();
        }

        let mut aliases = vec![self.own_nick.clone()];
        if self.plugin == "matrix" {
            if let Some(first_name) = self.own_nick.split_whitespace().next() {
                if first_name.chars().count() >= 3 {
                    aliases.push(first_name.to_owned());
                }
            }
            if let Some(own_member) = self
                .mention_candidates
                .iter()
                .find(|candidate| candidate.display_name == self.own_nick)
            {
                aliases.push(own_member.user_id.clone());
                if let Some(localpart) = own_member
                    .user_id
                    .strip_prefix('@')
                    .and_then(|id| id.split(':').next())
                {
                    aliases.push(localpart.to_owned());
                }
            }
        }
        aliases
    }
}

pub fn message_mentions_any_alias(message: &str, aliases: &[String]) -> bool {
    aliases
        .iter()
        .any(|alias| contains_word_case_insensitive(message, alias))
}

fn contains_word_case_insensitive(text: &str, needle: &str) -> bool {
    let text = text.to_lowercase();
    let needle = needle.to_lowercase();
    if needle.is_empty() {
        return false;
    }

    text.match_indices(&needle).any(|(start, matched)| {
        let end = start + matched.len();
        let left_is_word = text[..start]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
        let right_is_word = text[end..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
        !left_is_word && !right_is_word
    })
}

#[cfg(test)]
mod mention_highlight_tests {
    use super::*;

    fn matrix_buffer() -> Buffer {
        Buffer {
            id: "matrix/room".to_owned(),
            number: 1,
            name: "#postgis".to_owned(),
            full_name: "matrix.matrix.room".to_owned(),
            plugin: "matrix".to_owned(),
            kind: "channel".to_owned(),
            server: "matrix".to_owned(),
            own_nick: "Darafei Praliaskouski".to_owned(),
            messages: VecDeque::new(),
            nicks: Vec::new(),
            mention_candidates: vec![MentionCandidate {
                display_name: "Darafei Praliaskouski".to_owned(),
                user_id: "@komzpa:matrix.org".to_owned(),
            }],
            matrix_member_profiles: Vec::new(),
            activity: BufferActivity::None,
            unread_count: 0,
            last_read_id: None,
            last_markread_ts: None,
            topic: String::new(),
            modes: String::new(),
            hidden: false,
            muted: false,
            has_nicklist: true,
            matrix_room_id: Some("!room:example.org".to_owned()),
            matrix_predecessor_room_id: None,
            matrix_replacement_room_id: None,
            matrix_thread_root: None,
            matrix_upload_v1: true,
            matrix_avatar_mxc: None,
            visit_start_marker_id: None,
        }
    }

    #[test]
    fn matrix_display_name_and_mxid_aliases_highlight_own_mentions() {
        let buffer = matrix_buffer();
        let aliases = buffer.own_mention_aliases();
        assert!(message_mentions_any_alias(
            "whatever Darafei is using",
            &aliases
        ));
        assert!(message_mentions_any_alias("ping @komzpa:matrix.org", &aliases));
        assert!(message_mentions_any_alias("ping komzpa", &aliases));
        assert!(!message_mentions_any_alias(
            "a komzpapost is not a mention",
            &aliases
        ));
    }
}

#[derive(Debug, Clone, Copy)]
pub enum MatrixReplyLineKind {
    Header,
    Quote,
}

#[derive(Debug, Clone)]
pub struct MatrixReplyContext {
    pub event_id: Option<String>,
    pub sender: Option<String>,
    pub kind: MatrixReplyLineKind,
}

#[derive(Debug, Clone)]
pub struct MatrixMedia {
    pub mxc_uri: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MentionCandidate {
    pub display_name: String,
    pub user_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MatrixMemberProfile {
    pub user_id: String,
    pub display_name: String,
    pub nick: String,
    pub membership: String,
    pub role: String,
    pub power_level: Option<i64>,
    pub avatar_mxc: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Line {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub prefix: String,
    pub message: String,
    pub matrix_event_id: Option<String>,
    pub matrix_reply: Option<MatrixReplyContext>,
    pub matrix_media: Option<MatrixMedia>,
    pub displayed: bool,
    pub highlight: bool,
    // Cached: parsed once at insertion. Theme/font-independent — resolved at render.
    pub parsed_prefix: Vec<ANSISection>,
    pub parsed_message: Vec<ANSISection>,
    // Cached plain text (ANSI stripped) and lowercased copies for search.
    pub plain_prefix: String,
    pub plain_message: String,
    pub plain_prefix_lower: String,
    pub plain_message_lower: String,
}

impl Line {
    pub fn new(
        id: String,
        timestamp: DateTime<Utc>,
        prefix: String,
        message: String,
        displayed: bool,
        highlight: bool,
    ) -> Self {
        let parsed_prefix = ANSIParser::parse(&prefix);
        let parsed_message = ANSIParser::parse(&message);
        let plain_prefix: String = parsed_prefix.iter().map(|s| s.text.as_str()).collect();
        let plain_message: String = parsed_message.iter().map(|s| s.text.as_str()).collect();
        let plain_prefix_lower = plain_prefix.to_lowercase();
        let plain_message_lower = plain_message.to_lowercase();
        Self {
            id,
            timestamp,
            prefix,
            message,
            matrix_event_id: None,
            matrix_reply: None,
            matrix_media: None,
            displayed,
            highlight,
            parsed_prefix,
            parsed_message,
            plain_prefix,
            plain_message,
            plain_prefix_lower,
            plain_message_lower,
        }
    }

    pub fn with_matrix_media(mut self, matrix_media: Option<MatrixMedia>) -> Self {
        self.matrix_media = matrix_media;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Nick {
    pub name: String,
    pub prefix: String,
    pub color_ansi: String,
    pub away: bool,
}
