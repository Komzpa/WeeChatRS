use super::*;
use chrono::{TimeZone, Utc};
use std::collections::{HashSet, VecDeque};

fn line(id: &str, ts: i64, prefix: &str, message: &str, event_id: &str) -> Line {
    let mut line = Line::new(
        id.to_owned(),
        Utc.timestamp_opt(ts, 0).single().unwrap(),
        prefix.to_owned(),
        message.to_owned(),
        true,
        false,
    );
    line.matrix_event_id = Some(event_id.to_owned());
    line
}

fn buffer(id: &str, full_name: &str, plugin: &str, kind: &str) -> Buffer {
    Buffer {
        id: id.to_owned(),
        number: 1,
        name: full_name.to_owned(),
        full_name: full_name.to_owned(),
        plugin: plugin.to_owned(),
        kind: kind.to_owned(),
        server: plugin.to_owned(),
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
        has_nicklist: true,
        matrix_room_id: None,
        matrix_predecessor_room_id: None,
        matrix_replacement_room_id: None,
        matrix_thread_root: None,
        matrix_upload_v1: false,
        matrix_avatar_mxc: None,
        visit_start_marker_id: None,
    }
}

fn matrix_buffer(id: &str, room_id: &str) -> Buffer {
    let mut buffer = buffer(id, id, "matrix", "channel");
    buffer.matrix_room_id = Some(room_id.to_owned());
    buffer
}

fn profile(user_id: &str, display_name: &str, nick: &str) -> MatrixMemberProfile {
    MatrixMemberProfile {
        user_id: user_id.to_owned(),
        display_name: display_name.to_owned(),
        nick: nick.to_owned(),
        membership: "join".to_owned(),
        role: "member".to_owned(),
        power_level: Some(0),
        avatar_mxc: None,
    }
}

#[test]
fn harness_matrix_upgrade_sidebar_selection_and_history_are_coherent() {
    let mut old = matrix_buffer("local/old", "!old:example.org");
    old.matrix_replacement_room_id = Some("!new:example.org".to_owned());
    old.messages.push_back(line("old-1", 10, "alice", "old room", "$old"));

    let mut new = matrix_buffer("local/new", "!new:example.org");
    new.matrix_predecessor_room_id = Some("!old:example.org".to_owned());
    new.messages.push_back(line("new-1", 20, "bob", "new room", "$new"));

    let buffers = vec![old.clone(), new.clone()];
    let replaced = replaced_matrix_room_ids(&buffers);

    assert!(!buffer_visible_in_sidebar(&old, true, &HashSet::new(), &replaced));
    assert!(buffer_visible_in_sidebar(&new, true, &HashSet::new(), &replaced));
    assert_eq!(replacement_buffer_id(&buffers, "local/old").as_deref(), Some("local/new"));
    assert_eq!(
        preferred_chat_buffer_id(&buffers, Some("local/matrix.matrix.!old:example.org")).as_deref(),
        Some("local/new"),
    );

    let (messages, inherited) = composed_upgrade_history(&buffers, "local/new");
    assert_eq!(
        messages.iter().map(|line| line.id.as_str()).collect::<Vec<_>>(),
        ["upgrade-history:local/old:old-1", "new-1"],
    );
    assert!(inherited.contains("upgrade-history:local/old:old-1"));
}

#[test]
fn harness_legacy_reply_header_renders_as_body_context_not_raw_row() {
    let mut header = line(
        "reply-header",
        10,
        "bob",
        "Reply to $original:example.org",
        "$reply:example.org",
    );
    header.matrix_reply = Some(MatrixReplyContext {
        event_id: Some("$original:example.org".to_owned()),
        sender: None,
        kind: MatrixReplyLineKind::Header,
    });
    let body = line("reply-body", 11, "bob", "actual reply body", "$reply:example.org");
    let lines = VecDeque::from([header.clone(), body.clone()]);
    let contexts = reply_contexts_by_event(&lines);

    assert!(redundant_room_reply_header(&lines, &header));
    let context = room_reply_context_for_body_line(&body, &contexts)
        .expect("body row should carry reply context");
    assert!(context.has_header);
    assert_eq!(context.target_event_id.as_deref(), Some("$original:example.org"));
    assert_eq!(context.sender.as_deref(), None);
}

#[test]
fn harness_orphaned_reply_and_empty_room_history_do_not_disappear_into_spinners() {
    let mut orphan = line(
        "reply-header",
        10,
        "bob",
        "Reply to $original:example.org",
        "$reply:example.org",
    );
    orphan.matrix_reply = Some(MatrixReplyContext {
        event_id: Some("$original:example.org".to_owned()),
        sender: None,
        kind: MatrixReplyLineKind::Header,
    });

    assert!(!redundant_room_reply_header(&VecDeque::from([orphan.clone()]), &orphan));
    assert!(!should_auto_request_history(0, false, false));
    assert!(!should_auto_request_history(0, true, true));
    assert!(should_auto_request_history(20, false, false));
}

#[test]
fn harness_media_threads_and_mentions_keep_their_render_metadata() {
    let mut media_line = line("media", 10, "alice", "image", "$image:example.org");
    media_line.matrix_media = Some(MatrixMedia {
        mxc_uri: "mxc://example.org/screenshot".to_owned(),
        name: "screenshot.png".to_owned(),
        kind: "image".to_owned(),
    });
    let blocks = group_thread_lines(&VecDeque::from([media_line]));

    assert_eq!(blocks.len(), 1);
    assert_eq!(
        blocks[0].content[0].media.as_ref().map(|media| media.mxc_uri.as_str()),
        Some("mxc://example.org/screenshot"),
    );

    let profiles = vec![profile("@strk:osgeo.org", "strk 🧭", "strk 🧭")];
    let card = message_sender_profile_card(
        "matrix/room",
        "&strk 🧭",
        "matrix",
        true,
        &profiles,
        &[],
    )
    .expect("ranked Matrix sender should resolve exactly");
    assert_eq!(card.matrix_user_id.as_deref(), Some("@strk:osgeo.org"));

    let sections = ANSIParser::parse("\x1b[32mGrayShade\x1b[0m (@grayshade:dend.ro)");
    let compact = compact_matrix_prefix_sections("GrayShade (@grayshade:dend.ro)", &sections);
    assert_eq!(
        compact.iter().map(|section| section.text.as_str()).collect::<String>(),
        "GrayShade ·dend",
    );
}

#[test]
fn harness_url_preview_state_matches_current_shipped_behavior() {
    let sections = ANSIParser::parse(
        "see https://example.org/a and https://example.org/b",
    );
    let urls = sections
        .iter()
        .filter_map(|section| section.url.clone())
        .filter(|url| !WeeChatApp::is_image_url(url) && is_safe_public_url(url))
        .collect::<Vec<_>>();

    assert_eq!(urls, ["https://example.org/a", "https://example.org/b"]);
    assert!(urls.iter().all(|url| !WeeChatApp::is_image_url(url)));
}
