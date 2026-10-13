//! Session and tab ADDRESSES for the control CLI, resolved against a plain view of the mux.
//!
//! An index is a position, and positions move: a session created or killed in between a
//! listing and a command shifts every index after it. The selectors below let a script name
//! the thing itself.
//!
//! Session: `s4` (its id), `name:hub` (its display name), `hub` (a bare name, when no id
//! matches), `4` (an index in creation order, as `list-sessions` prints it).
//!
//! Tab: `s4:t13` (its id, anywhere in the mux), `<session>/<tab>` where `<session>` is any
//! session selector and `<tab>` is a tab id, an index in that session, a `name:` name, or a
//! bare name — and a plain `12` is an index in the active session, as before.
//!
//! A name that matches more than one thing is AMBIGUOUS, never "the first one": the caller
//! is about to act on the answer, and a guess there is a wrong pane typed into.

/// One session, as the resolver needs to see it.
pub struct SessionView<'a> {
    pub id: &'a str,
    pub name: Option<&'a str>,
    /// `(tab id, tab name)` in tab order.
    pub tabs: Vec<(&'a str, Option<&'a str>)>,
}

#[derive(Debug, PartialEq)]
pub enum SelErr {
    NotFound(String),
    Ambiguous(String),
}

impl SelErr {
    /// The machine-readable error code a response carries.
    pub fn code(&self) -> &'static str {
        match self {
            SelErr::NotFound(_) => "not_found",
            SelErr::Ambiguous(_) => "ambiguous",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            SelErr::NotFound(m) | SelErr::Ambiguous(m) => m,
        }
    }
}

/// Resolve a session selector to its position in `sessions`.
pub fn session(sessions: &[SessionView], sel: &str) -> Result<usize, SelErr> {
    if let Ok(i) = sel.parse::<usize>() {
        return (i < sessions.len())
            .then_some(i)
            .ok_or_else(|| SelErr::NotFound(format!("no session at index {i}")));
    }
    if let Some(name) = sel.strip_prefix("name:") {
        return by_name(sessions.iter().map(|s| (s.id, s.name)), name, "session");
    }
    if let Some(i) = sessions.iter().position(|s| s.id == sel) {
        return Ok(i);
    }
    by_name(sessions.iter().map(|s| (s.id, s.name)), sel, "session")
}

/// Resolve a tab selector to `(session position, tab position)`. `active` is the position of
/// the session a bare index refers to.
pub fn tab(sessions: &[SessionView], active: usize, sel: &str) -> Result<(usize, usize), SelErr> {
    if let Ok(i) = sel.parse::<usize>() {
        let n = sessions.get(active).map_or(0, |s| s.tabs.len());
        return (i < n)
            .then_some((active, i))
            .ok_or_else(|| SelErr::NotFound(format!("no tab at index {i}")));
    }
    for (si, s) in sessions.iter().enumerate() {
        if let Some(ti) = s.tabs.iter().position(|(id, _)| *id == sel) {
            return Ok((si, ti));
        }
    }
    let Some((sess, t)) = sel.split_once('/') else {
        return Err(SelErr::NotFound(format!(
            "no tab '{sel}' (use a tab id like s1:t0, <session>/<tab>, or an index)"
        )));
    };
    let si = session(sessions, sess)?;
    let tabs = &sessions[si].tabs;
    if let Ok(i) = t.parse::<usize>() {
        return (i < tabs.len())
            .then_some((si, i))
            .ok_or_else(|| SelErr::NotFound(format!("no tab at index {i} in '{sess}'")));
    }
    if let Some(ti) = tabs.iter().position(|(id, _)| *id == t) {
        return Ok((si, ti));
    }
    let name = t.strip_prefix("name:").unwrap_or(t);
    by_name(tabs.iter().copied(), name, "tab").map(|ti| (si, ti))
}

/// The single entry whose name is `name`.
fn by_name<'a>(
    items: impl Iterator<Item = (&'a str, Option<&'a str>)>,
    name: &str,
    what: &str,
) -> Result<usize, SelErr> {
    let hits: Vec<(usize, &str)> = items
        .enumerate()
        .filter(|(_, (_, n))| *n == Some(name))
        .map(|(i, (id, _))| (i, id))
        .collect();
    match hits.as_slice() {
        [(i, _)] => Ok(*i),
        [] => Err(SelErr::NotFound(format!("no {what} named '{name}'"))),
        many => Err(SelErr::Ambiguous(format!(
            "{what} name '{name}' matches {}: {} — address it by id",
            many.len(),
            many.iter()
                .map(|(_, id)| *id)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mux() -> Vec<SessionView<'static>> {
        vec![
            SessionView {
                id: "s0",
                name: None,
                tabs: vec![("s0:t0", None), ("s0:t1", Some("build"))],
            },
            SessionView {
                id: "s1",
                name: Some("hub"),
                tabs: vec![
                    ("s1:t0", Some("haiku")),
                    ("s1:t3", Some("dup")),
                    ("s1:t4", Some("dup")),
                ],
            },
            SessionView {
                id: "s4",
                name: Some("twin"),
                tabs: vec![("s4:t0", None)],
            },
            SessionView {
                id: "s5",
                name: Some("twin"),
                tabs: vec![("s5:t0", None)],
            },
            // A session NAMED like another's id: the id wins for a bare selector, and
            // `name:` reaches the name.
            SessionView {
                id: "s6",
                name: Some("s0"),
                tabs: vec![("s6:t0", None)],
            },
        ]
    }

    #[test]
    fn sessions_by_index_id_and_name() {
        let m = mux();
        assert_eq!(session(&m, "1"), Ok(1));
        assert_eq!(session(&m, "s4"), Ok(2));
        assert_eq!(session(&m, "name:hub"), Ok(1));
        assert_eq!(session(&m, "hub"), Ok(1));
        assert_eq!(session(&m, "s0"), Ok(0));
        assert_eq!(session(&m, "name:s0"), Ok(4));
    }

    #[test]
    fn an_ambiguous_session_name_lists_the_ids() {
        let m = mux();
        let e = session(&m, "name:twin").unwrap_err();
        assert_eq!(e.code(), "ambiguous");
        assert!(
            e.message().contains("s4") && e.message().contains("s5"),
            "{e:?}"
        );
        assert_eq!(session(&m, "twin").unwrap_err().code(), "ambiguous");
    }

    #[test]
    fn missing_sessions_are_not_found() {
        let m = mux();
        assert_eq!(session(&m, "9").unwrap_err().code(), "not_found");
        assert_eq!(session(&m, "nope").unwrap_err().code(), "not_found");
        assert_eq!(session(&m, "").unwrap_err().code(), "not_found");
    }

    #[test]
    fn tabs_by_id_index_and_path() {
        let m = mux();
        assert_eq!(tab(&m, 0, "1"), Ok((0, 1)));
        assert_eq!(
            tab(&m, 1, "1"),
            Ok((1, 1)),
            "a bare index is in the ACTIVE session"
        );
        assert_eq!(tab(&m, 0, "s1:t3"), Ok((1, 1)));
        assert_eq!(tab(&m, 0, "hub/haiku"), Ok((1, 0)));
        assert_eq!(tab(&m, 0, "s1/2"), Ok((1, 2)));
        assert_eq!(tab(&m, 0, "name:hub/s1:t4"), Ok((1, 2)));
        assert_eq!(tab(&m, 0, "s0/name:build"), Ok((0, 1)));
    }

    #[test]
    fn tab_errors_carry_codes() {
        let m = mux();
        assert_eq!(tab(&m, 0, "hub/dup").unwrap_err().code(), "ambiguous");
        assert_eq!(tab(&m, 0, "twin/0").unwrap_err().code(), "ambiguous");
        assert_eq!(tab(&m, 0, "hub/nope").unwrap_err().code(), "not_found");
        assert_eq!(tab(&m, 0, "7").unwrap_err().code(), "not_found");
        assert_eq!(tab(&m, 0, "haiku").unwrap_err().code(), "not_found");
    }
}
