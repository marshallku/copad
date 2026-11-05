//! Renders the autonomous-loop prompt for an existing todo. Embedded so the panel and CLI return
//! identical strings. Update this template and `~/.claude/loop-template.md` (the manual-fill
//! version) together when the protocol changes.

const TEMPLATE: &str = include_str!("loop_prompt_template.txt");

/// Substitute `{TITLE}` / `{WORKSPACE}` / `{ID}` in the embedded template in a single pass: chained
/// `replace()` would rewrite a title containing literal `{ID}` on the next pass, so LOOP NAME
/// could differ from the real title.
///
/// No escaping: the text lands in a prompt body Claude reads as data. Title is only validated as
/// non-empty, so it may hold shell metacharacters or markdown for the user's shell or renderer to
/// handle. Workspace is validated against `[A-Za-z0-9_\-.@]+`, id by `validate_id`.
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
            // Unknown sequence starting with `{`: emit the brace and keep scanning (a single byte,
            // so the split is char-safe).
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
        // A title containing literal `{ID}` must survive verbatim.
        let p = render("trim docs {ID}", "copad", "T-2026-001");
        assert!(p.contains("LOOP NAME: trim docs {ID}"));
        assert!(!p.contains("LOOP NAME: trim docs T-2026-001"));
        assert!(p.contains("TODO ID: T-2026-001"));
        assert!(p.contains("WORKSPACE: copad"));
    }

    #[test]
    fn render_passes_through_unknown_brace_sequences_in_inputs() {
        // `{FOO}` braces that are not real markers must round-trip verbatim.
        let p = render("with {FOO} brace", "ws", "id");
        assert!(p.contains("LOOP NAME: with {FOO} brace"));
    }
}
