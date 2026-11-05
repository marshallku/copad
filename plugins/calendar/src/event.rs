//! Calendar event payload — the wire shape published in `calendar.event_imminent` events and
//! returned from `calendar.list_events` / `calendar.event_details`.
//!
//! It carries more than meeting-prep needs (attendees, response status, organizer, location,
//! conference URL, description) because trigger `condition` expressions use them ("skip if
//! my_status == 'declined'", "only physical-location meetings").

use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    /// `recurringEventId` — same across all instances of a recurring
    /// event, what triggers match on for "this weekly meeting" patterns.
    pub recurring_id: Option<String>,
    pub title: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub all_day: bool,
    /// `accepted` / `declined` / `tentative` / `needsAction`. `None`
    /// when the owner is the organizer with no self-response recorded.
    pub my_response_status: Option<String>,
    pub attendees: Vec<Attendee>,
    pub organizer: Option<Person>,
    pub location: Option<String>,
    pub description: Option<String>,
    /// e.g. Google Meet URL pulled from `conferenceData.entryPoints`.
    pub conference_url: Option<String>,
    /// Direct link to the event on calendar.google.com.
    pub html_link: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub email: Option<String>,
    pub name: Option<String>,
    pub response_status: Option<String>,
    pub is_self: bool,
    pub is_organizer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub email: Option<String>,
    pub name: Option<String>,
}

/// `None` on missing required fields (cancelled events without `start`,
/// malformed responses).
pub fn from_gcal_json(raw: &Value) -> Option<CalendarEvent> {
    let id = raw.get("id")?.as_str()?.to_string();
    let title = raw
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("(no title)")
        .to_string();

    let (start_time, all_day) = parse_event_time(raw.get("start")?)?;
    let (end_time, _) = parse_event_time(raw.get("end")?)?;

    let recurring_id = raw
        .get("recurringEventId")
        .and_then(Value::as_str)
        .map(str::to_string);

    let html_link = raw
        .get("htmlLink")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let location = raw
        .get("location")
        .and_then(Value::as_str)
        .map(str::to_string);

    let description = raw
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);

    let conference_url = extract_conference_url(raw);

    let attendees: Vec<Attendee> = raw
        .get("attendees")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(parse_attendee).collect())
        .unwrap_or_default();

    let my_response_status = attendees
        .iter()
        .find(|a: &&Attendee| a.is_self)
        .and_then(|a| a.response_status.clone());

    let organizer = raw.get("organizer").and_then(parse_person);

    Some(CalendarEvent {
        id,
        recurring_id,
        title,
        start_time,
        end_time,
        all_day,
        my_response_status,
        attendees,
        organizer,
        location,
        description,
        conference_url,
        html_link,
    })
}

/// `{dateTime, timeZone}` or `{date}` (all-day form) → `(time, is_all_day)`.
///
/// **All-day events use process-local tz as the calendar's tz** — the
/// event node carries no IANA zone, and the calendar's own zone is
/// authoritative. Correct for the single-user-on-own-laptop case
/// (machine tz == calendar tz); travelling / multi-region users see
/// reminders shifted by the offset. See `docs/roadmap.md`.
fn parse_event_time(node: &Value) -> Option<(DateTime<Utc>, bool)> {
    if let Some(dt) = node.get("dateTime").and_then(Value::as_str) {
        return DateTime::parse_from_rfc3339(dt)
            .ok()
            .map(|d| (d.with_timezone(&Utc), false));
    }
    if let Some(date) = node.get("date").and_then(Value::as_str) {
        let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
        let naive_midnight = parsed.and_hms_opt(0, 0, 0)?;
        // `earliest()` picks one of an ambiguous (fall-back) midnight instead of panicking; a
        // midnight inside a DST gap yields None and drops the event.
        let local = Local.from_local_datetime(&naive_midnight).earliest()?;
        return Some((local.with_timezone(&Utc), true));
    }
    None
}

fn parse_attendee(node: &Value) -> Option<Attendee> {
    Some(Attendee {
        email: node
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string),
        name: node
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::to_string),
        response_status: node
            .get("responseStatus")
            .and_then(Value::as_str)
            .map(str::to_string),
        is_self: node.get("self").and_then(Value::as_bool).unwrap_or(false),
        is_organizer: node
            .get("organizer")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_person(node: &Value) -> Option<Person> {
    let email = node
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_string);
    let name = node
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::to_string);
    if email.is_none() && name.is_none() {
        return None;
    }
    Some(Person { email, name })
}

fn extract_conference_url(raw: &Value) -> Option<String> {
    let conf = raw.get("conferenceData")?;
    let entries = conf.get("entryPoints").and_then(Value::as_array)?;
    let video = entries.iter().find(|e| {
        e.get("entryPointType")
            .and_then(Value::as_str)
            .map(|t| t == "video")
            .unwrap_or(false)
    });
    let pick = video.or_else(|| entries.iter().find(|e| e.get("uri").is_some()));
    pick.and_then(|e| e.get("uri").and_then(Value::as_str).map(str::to_string))
}

/// Wire format. Field-for-field with `CalendarEvent` except `start_time`
/// and `end_time` serialize as RFC3339 strings so triggers can interpolate
/// them directly (instead of chrono structs).
pub fn to_json(e: &CalendarEvent) -> Value {
    json!({
        "id": e.id,
        "recurring_id": e.recurring_id,
        "title": e.title,
        "start_time": e.start_time.to_rfc3339(),
        "end_time": e.end_time.to_rfc3339(),
        "all_day": e.all_day,
        "my_response_status": e.my_response_status,
        "attendees": e.attendees.iter().map(|a| json!({
            "email": a.email,
            "name": a.name,
            "response_status": a.response_status,
            "is_self": a.is_self,
            "is_organizer": a.is_organizer,
        })).collect::<Vec<_>>(),
        "organizer": e.organizer.as_ref().map(|p| json!({ "email": p.email, "name": p.name })),
        "location": e.location,
        "description": e.description,
        "conference_url": e.conference_url,
        "html_link": e.html_link,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_event() {
        let raw = json!({
            "id": "abc123",
            "summary": "Standup",
            "start": { "dateTime": "2026-04-26T10:00:00+09:00" },
            "end": { "dateTime": "2026-04-26T10:30:00+09:00" },
            "htmlLink": "https://calendar.google.com/event?eid=abc",
        });
        let e = from_gcal_json(&raw).unwrap();
        assert_eq!(e.id, "abc123");
        assert_eq!(e.title, "Standup");
        assert!(!e.all_day);
        assert_eq!(e.start_time.to_rfc3339(), "2026-04-26T01:00:00+00:00");
        assert!(e.attendees.is_empty());
        assert!(e.my_response_status.is_none());
    }

    #[test]
    fn parses_all_day_event_in_local_timezone() {
        let raw = json!({
            "id": "x",
            "summary": "Holiday",
            "start": { "date": "2026-01-01" },
            "end": { "date": "2026-01-02" },
            "htmlLink": "",
        });
        let e = from_gcal_json(&raw).unwrap();
        assert!(e.all_day);
        // The all-day date must be interpreted as midnight in the
        // process's local timezone, NOT UTC midnight, so that
        // imminent-event scheduling fires at the right wall-clock
        // moment for users outside UTC.
        let expected_local_midnight = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let expected_utc = Local
            .from_local_datetime(&expected_local_midnight)
            .earliest()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(e.start_time, expected_utc);
    }

    #[test]
    fn extracts_my_response_status_from_self_attendee() {
        let raw = json!({
            "id": "x",
            "summary": "1on1",
            "start": { "dateTime": "2026-04-26T10:00:00Z" },
            "end": { "dateTime": "2026-04-26T11:00:00Z" },
            "htmlLink": "",
            "attendees": [
                { "email": "boss@x.com", "responseStatus": "accepted" },
                { "email": "me@x.com", "responseStatus": "tentative", "self": true },
            ],
        });
        let e = from_gcal_json(&raw).unwrap();
        assert_eq!(e.my_response_status.as_deref(), Some("tentative"));
        assert_eq!(e.attendees.len(), 2);
        assert!(e.attendees.iter().any(|a| a.is_self));
    }

    #[test]
    fn extracts_recurring_id() {
        let raw = json!({
            "id": "instance-x",
            "recurringEventId": "weekly-1on1",
            "summary": "Weekly 1:1",
            "start": { "dateTime": "2026-04-26T10:00:00Z" },
            "end": { "dateTime": "2026-04-26T11:00:00Z" },
            "htmlLink": "",
        });
        let e = from_gcal_json(&raw).unwrap();
        assert_eq!(e.recurring_id.as_deref(), Some("weekly-1on1"));
    }

    #[test]
    fn extracts_conference_url_prefers_video_entry() {
        let raw = json!({
            "id": "x",
            "summary": "Meet",
            "start": { "dateTime": "2026-04-26T10:00:00Z" },
            "end": { "dateTime": "2026-04-26T11:00:00Z" },
            "htmlLink": "",
            "conferenceData": {
                "entryPoints": [
                    { "entryPointType": "phone", "uri": "tel:+1-555" },
                    { "entryPointType": "video", "uri": "https://meet.google.com/abc-defg-hij" },
                ]
            }
        });
        let e = from_gcal_json(&raw).unwrap();
        assert_eq!(
            e.conference_url.as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
    }

    #[test]
    fn missing_id_returns_none() {
        let raw = json!({
            "summary": "x",
            "start": { "dateTime": "2026-04-26T10:00:00Z" },
            "end": { "dateTime": "2026-04-26T11:00:00Z" },
        });
        assert!(from_gcal_json(&raw).is_none());
    }

    #[test]
    fn malformed_datetime_returns_none() {
        let raw = json!({
            "id": "x",
            "summary": "x",
            "start": { "dateTime": "not-an-rfc3339" },
            "end": { "dateTime": "2026-04-26T11:00:00Z" },
        });
        assert!(from_gcal_json(&raw).is_none());
    }

    #[test]
    fn to_json_round_trip_preserves_fields() {
        let raw = json!({
            "id": "x",
            "recurringEventId": "r1",
            "summary": "Sync",
            "start": { "dateTime": "2026-04-26T10:00:00Z" },
            "end": { "dateTime": "2026-04-26T11:00:00Z" },
            "htmlLink": "https://example/x",
            "location": "Room 1",
            "description": "agenda",
            "attendees": [
                { "email": "me@x.com", "self": true, "responseStatus": "accepted" },
            ],
            "organizer": { "email": "lead@x.com", "displayName": "Lead" },
        });
        let e = from_gcal_json(&raw).unwrap();
        let v = to_json(&e);
        assert_eq!(v["id"], "x");
        assert_eq!(v["recurring_id"], "r1");
        assert_eq!(v["title"], "Sync");
        assert_eq!(v["my_response_status"], "accepted");
        assert_eq!(v["location"], "Room 1");
        assert_eq!(v["description"], "agenda");
        assert_eq!(v["organizer"]["email"], "lead@x.com");
        assert_eq!(v["all_day"], false);
    }
}
