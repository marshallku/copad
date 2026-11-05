//! Layered prompt assembly for `todo.start_requested` events: one self-contained prompt string,
//! so the trigger TOML stays `prompt = "{event.assembled_prompt}"`. (`event.prompt` on
//! `todo.start_requested` is the raw frontmatter field, NOT the layered output; see
//! `action_start` in main.rs.)
//!
//! Layers (top-down, all optional):
//! 1. global preamble: `<docs_root>/claude/global.md`
//! 2. workspace preamble: `<docs_root>/claude/workspaces/<ws>.md`
//! 3. todo instruction: the `prompt` field if set, else `# {title}\n\n{body}`
//! 4. linked_jira key reference
//! 5. linked_kb: full markdown of each path under docs_root, prefixed `## Reference: <path>`
//!
//! `<docs_root>` is `<COPAD_TODO_ROOT>/..`. Layer files are read at assembly time, so edits to
//! global.md / workspaces apply to the next `todo.start`. Errors are non-fatal: missing layers
//! are skipped and unreadable `linked_kb` paths warn and are omitted. The prompt is never empty
//! since title is always present.

use std::path::{Path, PathBuf};

use crate::todo::Todo;

/// Layered prompt: filesystem layers (when `docs_root` is `Some`) +
/// `todo_instruction` (`todo.prompt` if set, else title+body) + optional
/// linked_jira line + linked_kb references (when `docs_root` is `Some`).
/// `docs_root = None` skips both filesystem layers; the instruction
/// section always renders so the result is never empty.
pub fn assemble(todo: &Todo, docs_root: Option<&Path>) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(root) = docs_root {
        if let Some(content) = read_layer(&root.join("claude").join("global.md")) {
            sections.push(content);
        }
        if let Some(content) = read_layer(
            &root
                .join("claude")
                .join("workspaces")
                .join(format!("{}.md", todo.workspace)),
        ) {
            sections.push(content);
        }
    }

    sections.push(todo_instruction(todo));

    if let Some(jira) = &todo.linked_jira {
        sections.push(format!("Linked Jira ticket: {jira}"));
    }

    if let Some(root) = docs_root {
        for path in &todo.linked_kb {
            match resolve_within(root, path) {
                Some(abs) => {
                    // Symlink-ancestor check, as in the KB plugin
                    // (`copad-plugin-kb::kb::check_no_symlink_ancestors`): `resolve_within` only
                    // catches `..` / absolute paths in the frontmatter, not `<root>/alias -> /etc`.
                    // Walk every existing prefix of `abs` down from `root` and reject any symlink.
                    if !path_has_no_symlink_ancestors(root, &abs) {
                        eprintln!(
                            "[todo] linked_kb path {path:?} rejected: symlink ancestor \
                             would escape {}",
                            root.display()
                        );
                        continue;
                    }
                    match read_layer(&abs) {
                        Some(content) => {
                            sections.push(format!("## Reference: {path}\n\n{content}"));
                        }
                        None => {
                            eprintln!(
                                "[todo] linked_kb path {path:?} not readable under {} — skipped",
                                root.display()
                            );
                        }
                    }
                }
                None => {
                    eprintln!(
                        "[todo] linked_kb path {path:?} rejected: must be a relative path \
                         that stays under {} (no leading `/`, no `..` segments)",
                        root.display()
                    );
                }
            }
        }
    }

    sections.join("\n\n")
}

/// `lstat` walk so an `alias → /etc` symlink can't bypass the lexical
/// containment check (mirrors KB's defense).
fn path_has_no_symlink_ancestors(root: &Path, target: &Path) -> bool {
    let Ok(rel) = target.strip_prefix(root) else {
        return false;
    };
    let mut cursor = root.to_path_buf();
    for component in rel.components() {
        cursor.push(component);
        match std::fs::symlink_metadata(&cursor) {
            Ok(md) if md.file_type().is_symlink() => return false,
            // Non-existent component is fine — we only care about
            // existing symlinks. read_layer will catch the missing
            // file later.
            Ok(_) | Err(_) => continue,
        }
    }
    true
}

/// Lexical containment check (no `..` / absolute components) BEFORE reading, so a file created
/// later at a forbidden lexical location can't sneak in.
fn resolve_within(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return None;
    }
    use std::path::Component;
    for c in candidate.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(root.join(candidate))
}

fn todo_instruction(todo: &Todo) -> String {
    if let Some(p) = &todo.prompt
        && !p.trim().is_empty()
    {
        return p.trim_end().to_string();
    }
    if todo.body.trim().is_empty() {
        format!("# {}", todo.title)
    } else {
        format!("# {}\n\n{}", todo.title, todo.body.trim_end())
    }
}

/// `parent(todo_root)`. `None` when todo root has no parent — caller
/// passes that `None` to `assemble`, which then skips the filesystem
/// layers (todo_instruction + linked_jira still render).
pub fn docs_root_for(todo_root: &Path) -> Option<PathBuf> {
    todo_root.parent().map(Path::to_path_buf)
}

fn read_layer(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let trimmed = s.trim_end_matches(['\n', '\r']).to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::{Priority, Status};
    use std::fs;
    use tempfile::tempdir;

    fn fixture_todo() -> Todo {
        Todo {
            id: "T-1".into(),
            status: Status::Open,
            created: "2026-04-28T00:00:00Z".into(),
            workspace: "copad".into(),
            title: "Ship Phase 18.2".into(),
            body: "wire claude.start prompt seeding".into(),
            priority: Priority::Normal,
            due: None,
            linked_jira: None,
            linked_slack: Vec::new(),
            linked_kb: Vec::new(),
            tags: Vec::new(),
            prompt: None,
        }
    }

    #[test]
    fn assemble_falls_back_to_title_and_body_with_no_layers() {
        let out = assemble(&fixture_todo(), None);
        assert!(out.contains("# Ship Phase 18.2"));
        assert!(out.contains("wire claude.start prompt seeding"));
    }

    #[test]
    fn assemble_uses_explicit_prompt_when_set() {
        let mut t = fixture_todo();
        t.prompt = Some("Implement assemble() per spec.".into());
        let out = assemble(&t, None);
        assert!(out.starts_with("Implement assemble() per spec."));
        // Title/body are NOT included when prompt is explicit: `prompt` is the imperative, body
        // stays human-only.
        assert!(!out.contains("wire claude.start"));
    }

    #[test]
    fn assemble_includes_linked_jira_key() {
        let mut t = fixture_todo();
        t.linked_jira = Some("PROJ-42".into());
        let out = assemble(&t, None);
        assert!(out.contains("Linked Jira ticket: PROJ-42"));
    }

    #[test]
    fn assemble_layers_global_workspace_and_kb() {
        let dir = tempdir().unwrap();
        let docs = dir.path();
        fs::create_dir_all(docs.join("claude/workspaces")).unwrap();
        fs::write(docs.join("claude/global.md"), "GLOBAL_RULES\n").unwrap();
        fs::write(docs.join("claude/workspaces/copad.md"), "WORKSPACE_RULES\n").unwrap();
        fs::create_dir_all(docs.join("notes")).unwrap();
        fs::write(docs.join("notes/spec.md"), "SPEC_DETAILS\n").unwrap();

        let mut t = fixture_todo();
        t.linked_kb = vec!["notes/spec.md".into()];
        let out = assemble(&t, Some(docs));

        let pos_global = out.find("GLOBAL_RULES").expect("global missing");
        let pos_ws = out.find("WORKSPACE_RULES").expect("workspace missing");
        let pos_task = out.find("# Ship Phase 18.2").expect("task missing");
        let pos_ref = out.find("SPEC_DETAILS").expect("kb ref missing");
        assert!(pos_global < pos_ws);
        assert!(pos_ws < pos_task);
        assert!(pos_task < pos_ref);
        assert!(out.contains("## Reference: notes/spec.md"));
    }

    #[test]
    fn assemble_skips_missing_kb_path_without_failing() {
        let dir = tempdir().unwrap();
        let mut t = fixture_todo();
        t.linked_kb = vec!["missing/file.md".into()];
        let out = assemble(&t, Some(dir.path()));
        assert!(out.contains("# Ship Phase 18.2"));
        assert!(!out.contains("missing/file.md"));
    }

    #[test]
    fn assemble_rejects_absolute_linked_kb_paths() {
        let dir = tempdir().unwrap();
        let outside = dir.path().parent().unwrap().join("outside.md");
        let _ = fs::write(&outside, "SECRET\n");
        let mut t = fixture_todo();
        t.linked_kb = vec![outside.to_string_lossy().to_string()];
        let out = assemble(&t, Some(dir.path()));
        assert!(!out.contains("SECRET"), "absolute path must not be read");
        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn assemble_rejects_parent_dir_escape() {
        let dir = tempdir().unwrap();
        let outside = dir.path().parent().unwrap().join("outside-rel.md");
        let _ = fs::write(&outside, "ESCAPED\n");
        let mut t = fixture_todo();
        t.linked_kb = vec!["../outside-rel.md".into()];
        let out = assemble(&t, Some(dir.path()));
        assert!(!out.contains("ESCAPED"), "..-escape must not be read");
        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn assemble_rejects_symlink_ancestor_escape() {
        let dir = tempdir().unwrap();
        let secret_dir = dir.path().parent().unwrap().join("secret-dir");
        let _ = fs::create_dir_all(&secret_dir);
        let secret_file = secret_dir.join("creds.md");
        let _ = fs::write(&secret_file, "SYMLINK_ESCAPED\n");
        // Plant a symlink INSIDE the docs root that points to a
        // sibling directory. A purely lexical check would accept
        // `alias/creds.md` because no `..` appears.
        let alias = dir.path().join("alias");
        let _ = std::os::unix::fs::symlink(&secret_dir, &alias);

        let mut t = fixture_todo();
        t.linked_kb = vec!["alias/creds.md".into()];
        let out = assemble(&t, Some(dir.path()));
        assert!(
            !out.contains("SYMLINK_ESCAPED"),
            "symlink-ancestor escape must not be read"
        );
        let _ = fs::remove_file(&alias);
        let _ = fs::remove_file(&secret_file);
        let _ = fs::remove_dir_all(&secret_dir);
    }

    #[test]
    fn docs_root_for_returns_parent() {
        let p = Path::new("/home/u/docs/todos");
        assert_eq!(docs_root_for(p), Some(PathBuf::from("/home/u/docs")));
    }
}
