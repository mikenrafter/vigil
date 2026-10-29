//! stdio MCP front: five tools over `Vigil`.

use std::sync::Arc;

use crate::engine::Vigil;

/// Server `instructions`: under 512 characters; says to create a watch, then
/// call `keepalive` repeatedly until `next` says "act on events".
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
    fn instructions_fit_codex_and_tell_the_agent_the_loop() {
        // why: SPEC "Packaging": instructions under 512 characters (Codex
        //      reads the first 512) and must say: create a watch, then call
        //      keepalive repeatedly until `next` says "act on events".
        //      The e2e test checks the same text arrives over the wire.
        // proves: CLAIM(instructions)
        let text = server_instructions();
        assert!(text.chars().count() < 512, "{} chars", text.chars().count());
        for needle in ["watch_create", "keepalive", "act on events"] {
            assert!(text.contains(needle), "instructions must mention {needle:?}: {text}");
        }
    }
}
