//! Renders the autonomous-loop prompt for an existing todo.
//!
//! Embedded so the panel + CLI return identical strings without an
//! external file dependency. Update both this template and
//! `~/.claude/loop-template.md` together when the protocol changes —
//! the latter is the manual-fill version (LOOP NAME / WORKSPACE / GOAL
//! / FIRST ITERATION as user-fill slots), this one targets an existing
//! todo and pre-fills those slots from the todo's metadata.

const TEMPLATE: &str = include_str!("loop_prompt_template.txt");

/// Substitute `{TITLE}` / `{WORKSPACE}` / `{ID}` markers in the
/// embedded template. Single-pass — chained `replace()` would let a
/// title containing literal `{ID}` get rewritten by the next pass, so
/// the rendered prompt's LOOP NAME could silently differ from the
/// actual todo title. This walks the template once and only substitutes
/// markers that came from the template itself.
///
/// No escaping: the substituted text lands inside the prompt body that
/// Claude reads as data, not inside a shell or markdown context this
/// code interprets. Title is only validated as non-empty
/// (`Store::create` / `Store::update`); it can contain shell metacharacters
/// or markdown that the *user's* shell or rendering layer would have to
/// handle. Workspace is validated against `[A-Za-z0-9_\-.@]+`; id is
/// `T-<datetime>-<seq>` by `generate_id`, constrained by `validate_id`.
pub fn render(title: &str, workspace: &str, id: &str) -> String {
    let mut out = String::with_capacity(TEMPLATE.len() + title.len() + workspace.len() + id.len());
    let mut rest = TEMPLATE;
    while let Some(idx) = rest.find('{') {
        out.push_str(&rest[..idx]);
        rest = &rest[idx..];
        if let Some(stripped) = rest.strip_prefix("{TITLE}") {
            out.push_str(title);
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix("{WORKSPACE}") {
            out.push_str(workspace);
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix("{ID}") {
            out.push_str(id);
            rest = stripped;
        } else {
            // Unknown sequence starting with `{` — emit the brace verbatim
            // and continue scanning past it. `{` is a single byte so
            // splitting at byte index 1 is char-boundary-safe.
            out.push('{');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_all_three_markers() {
        let p = render("doc trim", "copad", "T-2026-001");
        assert!(p.contains("LOOP NAME: doc trim"));
        assert!(p.contains("WORKSPACE: copad"));
        assert!(p.contains("TODO ID: T-2026-001"));
        // No raw markers left.
        assert!(!p.contains("{TITLE}"));
        assert!(!p.contains("{WORKSPACE}"));
        assert!(!p.contains("{ID}"));
    }

    #[test]
    fn render_keeps_protocol_intact() {
        let p = render("x", "y", "z");
        assert!(p.contains("ScheduleWakeup"));
        assert!(p.contains("ACTIVE_SESSION"));
        assert!(p.contains("coctl todo update z"));
    }

    #[test]
    fn render_does_not_re_substitute_marker_text_inside_title() {
        // Title containing literal `{ID}` text should survive verbatim —
        // chained `replace()` would corrupt it on the second pass.
        let p = render("trim docs {ID}", "copad", "T-2026-001");
        assert!(p.contains("LOOP NAME: trim docs {ID}"));
        assert!(!p.contains("LOOP NAME: trim docs T-2026-001"));
        // Real template markers still substituted.
        assert!(p.contains("TODO ID: T-2026-001"));
        assert!(p.contains("WORKSPACE: copad"));
    }

    #[test]
    fn render_passes_through_unknown_brace_sequences_in_inputs() {
        // Inputs containing `{FOO}` style braces that are NOT one of
        // the three real markers must round-trip verbatim — we don't
        // re-scan substituted text for further substitutions.
        let p = render("with {FOO} brace", "ws", "id");
        assert!(p.contains("LOOP NAME: with {FOO} brace"));
    }
}
