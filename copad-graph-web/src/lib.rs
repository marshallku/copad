//! The comux work-graph page (decisions #204) as one self-contained HTML document.
//!
//! The page lives in `web/` (`graph.html`, `graph.css`, `model.js`, `graph.js`); this crate
//! inlines the stylesheet and both scripts into the HTML so a host can load it with no file
//! access and no network — copad-ffi hands it to the macOS app, copad-linux loads it into
//! WebKitGTK. The iOS app, which links no Rust, bundles the four files and does the same two
//! substitutions in Swift (`GraphPage.swift`); keep the markers in step.

pub const CSS_MARKER: &str = "/*COPAD_GRAPH_CSS*/";
/// Where the scripts go in `graph.html` (model first: the page script uses it).
pub const JS_MARKER: &str = "//COPAD_GRAPH_JS";

pub const HTML: &str = include_str!("../web/graph.html");
pub const CSS: &str = include_str!("../web/graph.css");
pub const MODEL_JS: &str = include_str!("../web/model.js");
pub const GRAPH_JS: &str = include_str!("../web/graph.js");

pub fn page() -> String {
    HTML.replacen(CSS_MARKER, CSS, 1)
        .replacen(JS_MARKER, &format!("{MODEL_JS}\n{GRAPH_JS}"), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_is_self_contained() {
        let p = page();
        assert!(!p.contains(CSS_MARKER) && !p.contains(JS_MARKER));
        assert!(p.contains("window.comuxGraph"));
        assert!(p.contains("const GraphModel"));
        // Nothing to fetch: no external script, stylesheet or image.
        for bad in ["<script src", "<link", "src=\"http", "url(http", "@import"] {
            assert!(
                !p.contains(bad),
                "the page references something external: {bad}"
            );
        }
        assert!(p.contains("default-src 'none'"));
    }

    #[test]
    fn a_script_end_tag_never_appears_inside_the_scripts() {
        // An inlined `</script>` would end the script element early.
        assert!(!MODEL_JS.contains("</script") && !GRAPH_JS.contains("</script"));
        assert!(!CSS.contains("</style"));
    }
}
