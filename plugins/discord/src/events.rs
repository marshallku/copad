//! Map Discord DISPATCH payloads to copad-shaped `discord.*` events.
//!
//! Two DISPATCH event types are handled:
//!
//! 1. `MESSAGE_CREATE` — fans out to up to two copad events:
//!    - `discord.raw` — always emitted (full firehose for archive triggers)
//!    - `discord.message` — regular guild channel message (filtered)
//!    - `discord.dm` — DM-channel message (no `guild_id`)
//!    - `discord.mention` — message that @-mentions the bot or @everyone
//!
//! 2. `MESSAGE_REACTION_ADD` — emits a single `discord.reaction`
//!    event. Useful for the "react with 📝 → capture as Todo" flow.
//!    Reactions don't carry the message body — capture triggers
//!    chain `discord.get_message` to fetch content. (Removal +
//!    burst/super reactions deferred — slice 2.5 captures the common
//!    case of a single user adding a single emoji.)
//!
//! Filter rules for MESSAGE_CREATE (apply to the non-raw events):
//! - `author.bot == true` — skip (avoids feedback loops with other bots)
//! - `author.id == bot_user_id` — skip (self-loop guard; READY's
//!   `user.id` is authoritative, falls back to stored id)
//! - `guild_id` absent → `discord.dm` instead of `discord.message`
//! - mention iff `mentions[].id` contains bot id OR `mention_everyone`
//!
//! Filter rules for MESSAGE_REACTION_ADD:
//! - `user_id == bot_user_id` — skip (own reactions, common when bot
//!   is configured to add a "starter" emoji that humans then mirror).
//!
//! Slice 2 (now 2.5 with reactions) restricts the allowlist; UPDATE,
//! DELETE, REACTION_REMOVE, GUILD_CREATE, PRESENCE_UPDATE etc. arrive
//! on the gateway but produce no copad events. Discord's DISPATCH
//! variety (especially PRESENCE_UPDATE bursts) would flood downstream
//! triggers without value, so we keep the allowlist explicit.
//!
//! Mentions in Discord come pre-resolved: the gateway delivers a
//! `mentions: [User, ...]` array on each message, so we don't need to
//! parse `<@id>` tokens out of the content body — `mentions[].id`
//! contains exactly the user IDs the message addresses.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscordEvent {
    Message(MessageFields),
    Dm(MessageFields),
    Mention(MessageFields),
    /// Reaction added to a message. Doesn't carry the original
    /// message body — chained `discord.get_message` fetches it.
    Reaction(ReactionFields),
    /// Full-fidelity firehose for MESSAGE_CREATE (entire `d` object).
    /// Mirrors `slack.raw`'s posture for archive triggers.
    Raw(RawEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFields {
    pub message_id: String,
    pub channel_id: String,
    /// `None` for DM channels (the absence is the DM signal).
    pub guild_id: Option<String>,
    /// Pre-built deep-link so triggers don't need string-concat in the
    /// interpolation DSL.
    pub message_url: String,
    pub author_id: String,
    /// `global_name` (display) preferred, `username` (handle) fallback so
    /// the field is always populated. Triggers needing a stable identifier
    /// should use `author_id` instead.
    pub author_username: String,
    pub content: String,
    pub mention_everyone: bool,
    /// Pre-computed: bot id in `mentions[]` OR `mention_everyone`. Lets
    /// triggers `payload_match` without nested-array inspection.
    pub mentions_bot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionFields {
    pub message_id: String,
    pub channel_id: String,
    /// `None` for DM channels.
    pub guild_id: Option<String>,
    /// Same format as `MessageFields::message_url`.
    pub message_url: String,
    /// User who added the reaction.
    pub user_id: String,
    /// Author of the underlying message when Discord supplies it (absent
    /// for older clients or deleted authors). Used as a guard so triggers
    /// don't chain `get_message` against the bot's own messages.
    pub message_author_id: Option<String>,
    /// Unicode codepoint for built-in emoji, custom name when `emoji_id`
    /// is set. Triggers typically `payload_match` on this.
    pub emoji_name: String,
    /// Custom emoji snowflake. `None` means a unicode emoji.
    pub emoji_id: Option<String>,
    pub emoji_animated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    pub event_type: String,
    pub channel_id: Option<String>,
    pub guild_id: Option<String>,
    pub message_id: Option<String>,
    /// Verbatim DISPATCH `d` object. Triggers access nested fields via
    /// the engine's dot-path interpolator (same posture as `slack.raw`).
    pub event_json: Value,
}

impl DiscordEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            DiscordEvent::Message(_) => "discord.message",
            DiscordEvent::Dm(_) => "discord.dm",
            DiscordEvent::Mention(_) => "discord.mention",
            DiscordEvent::Reaction(_) => "discord.reaction",
            DiscordEvent::Raw(_) => "discord.raw",
        }
    }

    pub fn payload_json(&self) -> Value {
        match self {
            DiscordEvent::Message(f) | DiscordEvent::Dm(f) | DiscordEvent::Mention(f) => json!({
                "message_id": f.message_id,
                "channel_id": f.channel_id,
                "guild_id": f.guild_id,
                "message_url": f.message_url,
                "author_id": f.author_id,
                "author_username": f.author_username,
                "content": f.content,
                "mention_everyone": f.mention_everyone,
                "mentions_bot": f.mentions_bot,
            }),
            DiscordEvent::Reaction(r) => json!({
                "message_id": r.message_id,
                "channel_id": r.channel_id,
                "guild_id": r.guild_id,
                "message_url": r.message_url,
                "user_id": r.user_id,
                "message_author_id": r.message_author_id,
                "emoji_name": r.emoji_name,
                "emoji_id": r.emoji_id,
                "emoji_animated": r.emoji_animated,
            }),
            DiscordEvent::Raw(r) => json!({
                "event_type": r.event_type,
                "channel_id": r.channel_id,
                "guild_id": r.guild_id,
                "message_id": r.message_id,
                "event_json": r.event_json,
            }),
        }
    }
}

/// `event_name` = DISPATCH `t`; `data` = DISPATCH `d`. `bot_user_id` is
/// the READY frame's `user.id` (authoritative) or stored TokenSet fallback,
/// used to filter the bot's own messages/reactions.
pub fn from_dispatch(
    event_name: &str,
    data: &Value,
    bot_user_id: Option<&str>,
) -> Vec<DiscordEvent> {
    match event_name {
        "MESSAGE_CREATE" => {
            let mut out = Vec::with_capacity(2);
            out.push(DiscordEvent::Raw(build_raw(event_name, data)));
            if let Some(filtered) = classify_message(data, bot_user_id) {
                out.push(filtered);
            }
            out
        }
        "MESSAGE_REACTION_ADD" => {
            // Reactions don't get a raw entry — `discord.raw` is
            // scoped to MESSAGE_CREATE for archive purposes. A user
            // who wants every reaction archived can layer `kb.append`
            // on `discord.reaction` directly; the filter pass below
            // is the only "noise gate" they need.
            classify_reaction(data, bot_user_id).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// `https://discord.com/channels/{guild_id|@me}/{channel_id}/{message_id}` —
/// `@me` for DM channels (what Discord's web client uses).
fn build_message_url(guild_id: Option<&str>, channel_id: &str, message_id: &str) -> String {
    let guild_segment = guild_id.unwrap_or("@me");
    format!("https://discord.com/channels/{guild_segment}/{channel_id}/{message_id}")
}

fn build_raw(event_name: &str, data: &Value) -> RawEvent {
    RawEvent {
        event_type: event_name.to_string(),
        channel_id: data
            .get("channel_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        guild_id: data
            .get("guild_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        message_id: data.get("id").and_then(Value::as_str).map(str::to_string),
        event_json: data.clone(),
    }
}

fn classify_message(data: &Value, bot_user_id: Option<&str>) -> Option<DiscordEvent> {
    let author = data.get("author")?;
    let author_id = author.get("id").and_then(Value::as_str)?.to_string();

    // Skip bot-authored messages — including our own (would loop) and
    // any other bots' chatter. The author.bot flag covers both cases
    // when `bot_user_id` is unknown (e.g. before READY arrives in a
    // pathological reconnect ordering); the explicit id check covers
    // the case where Discord ever stops setting the flag for our app.
    if author.get("bot").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    if let Some(bot_id) = bot_user_id
        && bot_id == author_id
    {
        return None;
    }

    let message_id = data.get("id").and_then(Value::as_str)?.to_string();
    let channel_id = data.get("channel_id").and_then(Value::as_str)?.to_string();
    let guild_id = data
        .get("guild_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let author_username = author
        .get("global_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| author.get("username").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let mention_everyone = data
        .get("mention_everyone")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mentions_bot = mention_everyone
        || bot_user_id
            .map(|bot_id| {
                data.get("mentions")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .any(|m| m.get("id").and_then(Value::as_str) == Some(bot_id))
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false);
    let message_url = build_message_url(guild_id.as_deref(), &channel_id, &message_id);
    let fields = MessageFields {
        message_id,
        channel_id,
        guild_id: guild_id.clone(),
        message_url,
        author_id,
        author_username,
        content,
        mention_everyone,
        mentions_bot,
    };
    // Mention takes precedence: a single MESSAGE_CREATE can be both
    // a mention AND a guild/DM message. We emit ONE filtered event so
    // the trigger DSL stays simple. A user who wants "every message
    // including mentions" registers two triggers — one on
    // `discord.message`, one on `discord.mention` — both with the
    // same action body. They are NOT recoverable via `mentions_bot`
    // payload-match on `discord.message` because mention messages
    // never produce a `discord.message` event in the first place.
    // The `mentions_bot` field still exists on the payload for
    // diagnostics and for `discord.dm + condition` filtering, where
    // a DM that is also a mention won't reach the .dm trigger anyway
    // but the field tells you why.
    if mentions_bot {
        return Some(DiscordEvent::Mention(fields));
    }
    if guild_id.is_none() {
        return Some(DiscordEvent::Dm(fields));
    }
    Some(DiscordEvent::Message(fields))
}

fn classify_reaction(data: &Value, bot_user_id: Option<&str>) -> Option<DiscordEvent> {
    let user_id = data.get("user_id").and_then(Value::as_str)?.to_string();
    // Filter out the bot's own reactions. Common when a workflow
    // posts a starter emoji on every message; without this guard the
    // starter reaction itself triggers the capture pipeline.
    if let Some(bot_id) = bot_user_id
        && bot_id == user_id
    {
        return None;
    }
    let message_id = data.get("message_id").and_then(Value::as_str)?.to_string();
    let channel_id = data.get("channel_id").and_then(Value::as_str)?.to_string();
    let guild_id = data
        .get("guild_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let message_author_id = data
        .get("message_author_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let emoji = data.get("emoji")?;
    let emoji_name = emoji.get("name").and_then(Value::as_str)?.to_string();
    let emoji_id = emoji.get("id").and_then(Value::as_str).map(str::to_string);
    let emoji_animated = emoji
        .get("animated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message_url = build_message_url(guild_id.as_deref(), &channel_id, &message_id);
    Some(DiscordEvent::Reaction(ReactionFields {
        message_id,
        channel_id,
        guild_id,
        message_url,
        user_id,
        message_author_id,
        emoji_name,
        emoji_id,
        emoji_animated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(event_name: &str, data: Value) -> Vec<DiscordEvent> {
        from_dispatch(event_name, &data, Some("BOT_USER_ID"))
    }

    fn message_data(overrides: Value) -> Value {
        let mut base = json!({
            "id": "MSG_1",
            "channel_id": "CH_1",
            "guild_id": "G_1",
            "author": {"id": "U_1", "username": "alice", "global_name": "Alice"},
            "content": "hello",
            "mention_everyone": false,
            "mentions": [],
        });
        if let (Some(b), Some(o)) = (base.as_object_mut(), overrides.as_object()) {
            for (k, v) in o {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn expect_raw(out: &[DiscordEvent]) -> &RawEvent {
        match &out[0] {
            DiscordEvent::Raw(r) => r,
            other => panic!("expected Raw first, got {other:?}"),
        }
    }

    #[test]
    fn regular_message_emits_message_and_raw() {
        let out = dispatch("MESSAGE_CREATE", message_data(json!({})));
        assert_eq!(out.len(), 2);
        let raw = expect_raw(&out);
        assert_eq!(raw.event_type, "MESSAGE_CREATE");
        assert_eq!(raw.channel_id.as_deref(), Some("CH_1"));
        match &out[1] {
            DiscordEvent::Message(f) => {
                assert_eq!(f.message_id, "MSG_1");
                assert_eq!(f.channel_id, "CH_1");
                assert_eq!(f.guild_id.as_deref(), Some("G_1"));
                assert_eq!(f.message_url, "https://discord.com/channels/G_1/CH_1/MSG_1");
                assert_eq!(f.author_id, "U_1");
                assert_eq!(f.author_username, "Alice");
                assert_eq!(f.content, "hello");
                assert!(!f.mentions_bot);
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn dm_emits_dm_and_raw() {
        let out = dispatch(
            "MESSAGE_CREATE",
            message_data(json!({"guild_id": Value::Null})),
        );
        assert_eq!(out.len(), 2);
        match &out[1] {
            DiscordEvent::Dm(f) => {
                assert_eq!(f.guild_id, None);
                assert_eq!(f.channel_id, "CH_1");
                // DMs use Discord's `@me` URL convention.
                assert_eq!(f.message_url, "https://discord.com/channels/@me/CH_1/MSG_1");
            }
            other => panic!("expected Dm, got {other:?}"),
        }
    }

    #[test]
    fn dm_without_guild_id_field_at_all_still_dm() {
        let mut data = message_data(json!({}));
        data.as_object_mut().unwrap().remove("guild_id");
        let out = from_dispatch("MESSAGE_CREATE", &data, Some("BOT_USER_ID"));
        match &out[1] {
            DiscordEvent::Dm(_) => {}
            other => panic!("expected Dm, got {other:?}"),
        }
    }

    #[test]
    fn bot_message_emits_only_raw() {
        let out = dispatch(
            "MESSAGE_CREATE",
            message_data(json!({
                "author": {"id": "OTHER_BOT", "username": "otherbot", "bot": true},
            })),
        );
        assert_eq!(out.len(), 1, "bot messages must be raw-only, got {out:?}");
        assert!(matches!(out[0], DiscordEvent::Raw(_)));
    }

    #[test]
    fn self_message_emits_only_raw() {
        let out = dispatch(
            "MESSAGE_CREATE",
            message_data(json!({
                "author": {"id": "BOT_USER_ID", "username": "self"},
            })),
        );
        assert_eq!(out.len(), 1, "self messages must be raw-only");
        assert!(matches!(out[0], DiscordEvent::Raw(_)));
    }

    #[test]
    fn mention_via_mentions_array_emits_mention() {
        let out = dispatch(
            "MESSAGE_CREATE",
            message_data(json!({
                "mentions": [{"id": "BOT_USER_ID", "username": "copad"}],
                "content": "<@BOT_USER_ID> ping?",
            })),
        );
        assert_eq!(out.len(), 2);
        match &out[1] {
            DiscordEvent::Mention(f) => {
                assert!(f.mentions_bot);
                assert_eq!(f.content, "<@BOT_USER_ID> ping?");
            }
            other => panic!("expected Mention, got {other:?}"),
        }
    }

    #[test]
    fn everyone_mention_emits_mention_even_without_bot_in_array() {
        let out = dispatch(
            "MESSAGE_CREATE",
            message_data(json!({
                "mention_everyone": true,
                "content": "@everyone heads up",
            })),
        );
        match &out[1] {
            DiscordEvent::Mention(f) => {
                assert!(f.mention_everyone);
                assert!(f.mentions_bot, "@everyone counts as a bot mention");
            }
            other => panic!("expected Mention, got {other:?}"),
        }
    }

    #[test]
    fn mention_without_bot_id_known_falls_back_to_message() {
        // No bot_user_id known yet (e.g. before READY) — even if the
        // mentions array contains some user, we can't safely classify
        // as bot-mention. Falls back to plain message.
        let out = from_dispatch(
            "MESSAGE_CREATE",
            &message_data(json!({
                "mentions": [{"id": "U_OTHER", "username": "other"}],
            })),
            None,
        );
        match &out[1] {
            DiscordEvent::Message(f) => assert!(!f.mentions_bot),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn mention_in_dm_classified_as_mention_not_dm() {
        // Mention precedence: a DM that @-mentions the bot fires
        // discord.mention, not discord.dm. Documented in the
        // classify comment above.
        let out = dispatch(
            "MESSAGE_CREATE",
            message_data(json!({
                "guild_id": Value::Null,
                "mentions": [{"id": "BOT_USER_ID", "username": "copad"}],
            })),
        );
        match &out[1] {
            DiscordEvent::Mention(f) => {
                assert_eq!(f.guild_id, None);
                assert!(f.mentions_bot);
            }
            other => panic!("expected Mention, got {other:?}"),
        }
    }

    #[test]
    fn non_message_create_dispatch_returns_empty() {
        // Slice 2 only handles MESSAGE_CREATE. PRESENCE_UPDATE,
        // GUILD_CREATE, etc. produce no copad event.
        let out = dispatch("PRESENCE_UPDATE", json!({"foo": "bar"}));
        assert!(out.is_empty());
    }

    #[test]
    fn raw_preserves_full_payload() {
        let data = message_data(json!({
            "embeds": [{"type": "rich", "title": "T"}],
            "attachments": [{"id": "A1", "filename": "x.png"}],
            "components": [{"type": 1}],
        }));
        let out = from_dispatch("MESSAGE_CREATE", &data, Some("BOT_USER_ID"));
        let raw = expect_raw(&out);
        assert_eq!(raw.event_json["embeds"][0]["title"], "T");
        assert_eq!(raw.event_json["attachments"][0]["id"], "A1");
        assert_eq!(raw.event_json["components"][0]["type"], 1);
    }

    #[test]
    fn missing_author_returns_raw_only() {
        let mut data = message_data(json!({}));
        data.as_object_mut().unwrap().remove("author");
        let out = from_dispatch("MESSAGE_CREATE", &data, Some("BOT_USER_ID"));
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], DiscordEvent::Raw(_)));
    }

    #[test]
    fn global_name_falls_back_to_username() {
        let out = dispatch(
            "MESSAGE_CREATE",
            message_data(json!({
                "author": {"id": "U_X", "username": "legacyname"},
            })),
        );
        match &out[1] {
            DiscordEvent::Message(f) => {
                assert_eq!(f.author_username, "legacyname");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn empty_global_name_falls_back_to_username() {
        let out = dispatch(
            "MESSAGE_CREATE",
            message_data(json!({
                "author": {"id": "U_X", "username": "fallback", "global_name": ""},
            })),
        );
        match &out[1] {
            DiscordEvent::Message(f) => assert_eq!(f.author_username, "fallback"),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn payload_json_includes_all_fields() {
        let f = MessageFields {
            message_id: "M".into(),
            channel_id: "C".into(),
            guild_id: Some("G".into()),
            message_url: "https://discord.com/channels/G/C/M".into(),
            author_id: "A".into(),
            author_username: "name".into(),
            content: "c".into(),
            mention_everyone: false,
            mentions_bot: true,
        };
        let v = DiscordEvent::Message(f).payload_json();
        assert_eq!(v["message_id"], "M");
        assert_eq!(v["channel_id"], "C");
        assert_eq!(v["guild_id"], "G");
        assert_eq!(v["message_url"], "https://discord.com/channels/G/C/M");
        assert_eq!(v["author_id"], "A");
        assert_eq!(v["mentions_bot"], true);
    }

    fn reaction_data(overrides: Value) -> Value {
        let mut base = json!({
            "user_id": "U_REACTOR",
            "channel_id": "CH_1",
            "guild_id": "G_1",
            "message_id": "MSG_1",
            "message_author_id": "U_AUTHOR",
            "emoji": {"id": null, "name": "📝", "animated": false},
        });
        if let (Some(b), Some(o)) = (base.as_object_mut(), overrides.as_object()) {
            for (k, v) in o {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    #[test]
    fn unicode_reaction_emits_discord_reaction() {
        let out = dispatch("MESSAGE_REACTION_ADD", reaction_data(json!({})));
        assert_eq!(out.len(), 1, "reactions don't fan out to raw");
        match &out[0] {
            DiscordEvent::Reaction(r) => {
                assert_eq!(r.user_id, "U_REACTOR");
                assert_eq!(r.message_id, "MSG_1");
                assert_eq!(r.channel_id, "CH_1");
                assert_eq!(r.guild_id.as_deref(), Some("G_1"));
                assert_eq!(r.message_url, "https://discord.com/channels/G_1/CH_1/MSG_1");
                assert_eq!(r.message_author_id.as_deref(), Some("U_AUTHOR"));
                assert_eq!(r.emoji_name, "📝");
                assert!(r.emoji_id.is_none(), "unicode emoji has no id");
                assert!(!r.emoji_animated);
            }
            other => panic!("expected Reaction, got {other:?}"),
        }
    }

    #[test]
    fn dm_reaction_url_uses_at_me_segment() {
        let out = dispatch(
            "MESSAGE_REACTION_ADD",
            reaction_data(json!({"guild_id": Value::Null})),
        );
        match &out[0] {
            DiscordEvent::Reaction(r) => {
                assert_eq!(r.message_url, "https://discord.com/channels/@me/CH_1/MSG_1");
            }
            other => panic!("expected Reaction, got {other:?}"),
        }
    }

    #[test]
    fn build_message_url_handles_guild_and_dm() {
        assert_eq!(
            build_message_url(Some("999"), "888", "777"),
            "https://discord.com/channels/999/888/777"
        );
        assert_eq!(
            build_message_url(None, "888", "777"),
            "https://discord.com/channels/@me/888/777"
        );
    }

    #[test]
    fn custom_emoji_reaction_carries_id() {
        let out = dispatch(
            "MESSAGE_REACTION_ADD",
            reaction_data(json!({
                "emoji": {"id": "987654321", "name": "partyparrot", "animated": true},
            })),
        );
        match &out[0] {
            DiscordEvent::Reaction(r) => {
                assert_eq!(r.emoji_name, "partyparrot");
                assert_eq!(r.emoji_id.as_deref(), Some("987654321"));
                assert!(r.emoji_animated);
            }
            other => panic!("expected Reaction, got {other:?}"),
        }
    }

    #[test]
    fn dm_reaction_has_no_guild_id() {
        let out = dispatch(
            "MESSAGE_REACTION_ADD",
            reaction_data(json!({"guild_id": Value::Null})),
        );
        match &out[0] {
            DiscordEvent::Reaction(r) => assert!(r.guild_id.is_none()),
            other => panic!("expected Reaction, got {other:?}"),
        }
    }

    #[test]
    fn self_reaction_filtered() {
        let out = dispatch(
            "MESSAGE_REACTION_ADD",
            reaction_data(json!({"user_id": "BOT_USER_ID"})),
        );
        assert!(
            out.is_empty(),
            "bot's own reactions must be filtered to avoid loops"
        );
    }

    #[test]
    fn missing_user_id_drops_reaction() {
        let mut data = reaction_data(json!({}));
        data.as_object_mut().unwrap().remove("user_id");
        let out = from_dispatch("MESSAGE_REACTION_ADD", &data, Some("BOT_USER_ID"));
        assert!(out.is_empty());
    }

    #[test]
    fn missing_emoji_drops_reaction() {
        let mut data = reaction_data(json!({}));
        data.as_object_mut().unwrap().remove("emoji");
        let out = from_dispatch("MESSAGE_REACTION_ADD", &data, Some("BOT_USER_ID"));
        assert!(out.is_empty());
    }

    #[test]
    fn message_reaction_remove_returns_empty() {
        // Not in slice 2.5's allowlist — handled separately if/when
        // a "uncapture on emoji removal" workflow comes up.
        let out = dispatch("MESSAGE_REACTION_REMOVE", reaction_data(json!({})));
        assert!(out.is_empty());
    }

    #[test]
    fn reaction_payload_json_round_trips() {
        let r = ReactionFields {
            message_id: "M".into(),
            channel_id: "C".into(),
            guild_id: None,
            message_url: "https://discord.com/channels/@me/C/M".into(),
            user_id: "U".into(),
            message_author_id: Some("A".into()),
            emoji_name: "🔥".into(),
            emoji_id: None,
            emoji_animated: false,
        };
        let v = DiscordEvent::Reaction(r).payload_json();
        assert_eq!(v["message_id"], "M");
        assert_eq!(v["channel_id"], "C");
        assert_eq!(v["guild_id"], Value::Null);
        assert_eq!(v["message_url"], "https://discord.com/channels/@me/C/M");
        assert_eq!(v["user_id"], "U");
        assert_eq!(v["message_author_id"], "A");
        assert_eq!(v["emoji_name"], "🔥");
        assert_eq!(v["emoji_id"], Value::Null);
        assert_eq!(v["emoji_animated"], false);
    }
}
