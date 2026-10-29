//! Built-in skills: read-only guides compiled into the binary and served
//! through the agent's file tools under `skills/`, alongside its per-origin
//! files. The system prompt only names them; the agent `read_file`s one when a
//! task needs it, so the API details and worked examples cost context only on
//! the runs that use them.

pub const PREFIX: &str = "skills/";

const SKILLS: &[(&str, &str)] = &[
    ("skills/file-analysis.md", include_str!("skills/file-analysis.md")),
    ("skills/human-input.md", include_str!("skills/human-input.md")),
];

/// Content of the built-in skill at `path`, if there is one.
pub fn get(path: &str) -> Option<&'static str> {
    SKILLS.iter().find(|(p, _)| *p == path).map(|(_, c)| *c)
}

/// Built-in skills whose path starts with `prefix`.
pub fn matching(prefix: &str) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
    SKILLS.iter().copied().filter(move |(p, _)| p.starts_with(prefix))
}

/// `skills/` belongs to the built-ins; the agent can't write there.
pub fn is_reserved(path: &str) -> bool {
    path.starts_with(PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_builtin_skills() {
        assert!(get("skills/human-input.md").unwrap().contains("human.click"));
        assert!(get("skills/file-analysis.md").unwrap().contains("run_command"));
        assert!(get("skills/nope.md").is_none());
        assert_eq!(matching("").count(), SKILLS.len());
        assert_eq!(matching("skills/human").count(), 1);
        assert_eq!(matching("notes").count(), 0);
        assert!(is_reserved("skills/mine.md"));
        assert!(!is_reserved("discover/scraper.js"));
    }
}
