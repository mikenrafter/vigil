//! stdio MCP front: five tools over `Vigil`.

use std::sync::Arc;

use crate::engine::Vigil;

/// Server `instructions`: under 512 characters; says to create a watch, then
/// call `keepalive` repeatedly, and (V20) to stop calling when `next` is not
/// exactly "call keepalive again" ("act on events" means act on them; the
/// ended text means create a new watch only if still needed).
pub fn server_instructions() -> String {
    todo!()
}

/// Serve on stdin/stdout until the client disconnects.
pub async fn serve_stdio(vigil: Arc<Vigil>) -> anyhow::Result<()> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_fit_codex_and_teach_the_loop_and_every_stop_condition() {
        // why: SPEC "Packaging": instructions under 512 characters (Codex
        //      reads the first 512) and must say: create a watch, then call
        //      keepalive repeatedly. V20 (S20): the old text said "until act on
        //      events", so an expired or cancelled watch would loop forever.
        //      The text must name the continue value verbatim and tell the
        //      agent to stop when `next` is anything else, and mention both
        //      other values. The e2e test checks the same text over the wire.
        // proves: CLAIM(instructions)
        let text = server_instructions();
        assert!(text.chars().count() < 512, "{} chars", text.chars().count());
        for needle in ["watch_create", "keepalive", "call keepalive again", "act on events", "watch ended"] {
            assert!(text.contains(needle), "instructions must mention {needle:?}: {text}");
        }
        assert!(text.to_lowercase().contains("stop"), "must say when to stop: {text}");
    }
}
