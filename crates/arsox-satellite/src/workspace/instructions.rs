// Copyright © 2026 Jalapeno Labs

//! The instruction files every thread's workspace carries.
//!
//! `AGENTS.md` is the real file. `CLAUDE.md` and `CODEX.md` are pointers at it,
//! which is what makes one set of instructions reach the runner whichever
//! harness is underneath. Writing three files instead of one is the price of
//! that: each harness looks for a name of its own, and neither looks for the
//! other's.
//!
//! # Precedence
//!
//! `AGENTS.md` is assembled from ordered layers, most general first:
//!
//! 1. **The Arsox header.** System-level facts about the satellite, and not
//!    overridable.
//! 2. **The agents repo.** Fleet-wide conventions. Not built yet, and the seam
//!    it slots into is [`layers`].
//! 3. **The thread's `prompt` setting.** Task-specific instruction.
//!
//! Most specific wins, so a thread can override its fleet and the fleet can
//! override nothing the satellite needs to hold. Every layer below the header is
//! advisory: it shapes what agents do and never constrains it. Anything that
//! must hold is enforced elsewhere, by infrastructure an agent cannot reach.

use super::WorkspaceError;
use arsox_sdk::proto::settings::v1::ThreadSettings;
use std::path::Path;

/// The file both harnesses are pointed at, and the only one worth reading.
const AGENTS_FILE: &str = "AGENTS.md";

/// The per-harness names that point at [`AGENTS_FILE`].
///
/// Both carry identical text. They exist because Claude reads `CLAUDE.md` and
/// Codex reads `CODEX.md`, and a satellite that guessed wrong would run an agent
/// with no instruction at all.
const POINTER_FILES: [&str; 2] = ["CLAUDE.md", "CODEX.md"];

/// The facts the satellite states about itself, above anything an operator says.
///
/// Deliberately short: every line is read by every agent on every turn, and a
/// header that rambles is a header that gets skimmed. Each line earns its place.
///
/// - **The workspace path**, because an agent that does not know where it is
///   writes into whatever directory its process happened to start in, and the
///   only durable subtree on the volume is this one.
/// - **The thread id**, because it names the workspace directory and is what an
///   operator quotes when asking what a run did.
/// - **`repos/`**, because a repo is cloned exactly once and an agent that goes
///   looking for its own checkout finds nothing.
/// - **What follows the header**, because the satellite's facts and the
///   operator's instruction end up in one file, and an agent that cannot tell
///   them apart cannot tell which of the two it may argue with.
///
/// `{workspace}` and `{thread_id}` are substituted when the file is assembled.
const ARSOX_HEADER: &str = "\
# Arsox

You are running on an Arsox satellite. This section is written by the satellite
itself and is not overridable.

- Your workspace is `{workspace}`, and this file is `{workspace}/AGENTS.md`.
- This thread is `{thread_id}`. It names your workspace directory and every
  artifact you leave behind.
- Repositories are cloned once each into `{workspace}/repos/`.
- Everything below this section is instruction from the operator who created
  this thread.
";

/// What `CLAUDE.md` and `CODEX.md` say, whichever harness reads them.
///
/// The bare `@` reference on its own line is how both harnesses import another
/// file, so it must not be wrapped in backticks or prose.
///
/// `{workspace}` is substituted when the file is written.
const POINTER: &str = "\
# Arsox

This file is a pointer. Every instruction for this thread lives in `AGENTS.md`,
which the reference below imports.

@{workspace}/AGENTS.md
";

/// Writes `AGENTS.md` and the harness pointers into a thread's workspace.
///
/// Overwrites rather than merges, so rebuilding a workspace after a restart
/// leaves exactly the three files a first provisioning would have.
///
/// Deliberately knows nothing about the database, like everything else that
/// fills a workspace: it takes a root and the thread's settings, so it can be
/// exercised against a temporary directory with no store and no bus.
///
/// # Errors
///
/// Returns [`WorkspaceError::UnsafeThreadId`] for an id that is not a plain
/// UUID, [`WorkspaceError::Create`] when the workspace directory cannot be
/// created, and [`WorkspaceError::Write`] when a file cannot be written.
pub async fn write_instructions(
    root: &Path,
    thread_id: &str,
    settings: &ThreadSettings,
) -> Result<(), WorkspaceError> {
    // The real directory rather than a hardcoded `/workspace`, so what an agent
    // reads is where it actually is. Inside the container the root is
    // `/workspace` and this resolves to `/workspace/<thread-id>`; under a test
    // it resolves to a temporary directory, and both are true statements.
    let workspace = super::thread_directory(root, thread_id)?;
    super::create_directory(&workspace).await?;

    let assembled = assemble(&layers(&workspace, thread_id, settings));
    write(&workspace.join(AGENTS_FILE), &assembled).await?;

    let pointer = POINTER.replace("{workspace}", &as_text(&workspace));
    for name in POINTER_FILES {
        write(&workspace.join(name), &pointer).await?;
    }

    tracing::debug!(
        event.name = "workspace.instructions.written",
        thread.id = thread_id,
        file.directory = %as_text(&workspace),
        "wrote AGENTS.md and the harness pointers",
    );

    Ok(())
}

/// The ordered layers of `AGENTS.md`, most general first.
///
/// This is the seam. The agents repo's fleet-wide conventions belong between the
/// header and the prompt, and adding them means pushing one more layer here
/// rather than touching how the file is assembled or written.
fn layers(workspace: &Path, thread_id: &str, settings: &ThreadSettings) -> Vec<String> {
    let mut ordered = vec![
        ARSOX_HEADER
            .replace("{workspace}", &as_text(workspace))
            .replace("{thread_id}", thread_id),
    ];

    // An absent prompt writes the header alone. An empty section under a
    // heading nobody wrote would read as instruction that went missing.
    if !settings.prompt.trim().is_empty() {
        ordered.push(settings.prompt.clone());
    }

    ordered
}

/// Joins ordered layers into the file the harnesses read.
///
/// Layers are trimmed and separated by a blank line, so the file reads the same
/// whether or not an operator's prompt happened to arrive with trailing
/// newlines. Empty layers are dropped rather than leaving a gap that looks like
/// something failed to render.
fn assemble(layers: &[String]) -> String {
    let mut assembled = layers
        .iter()
        .map(|layer| layer.trim())
        .filter(|layer| !layer.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    assembled.push('\n');
    assembled
}

/// A path as an agent should read it.
///
/// Named rather than inlined because it appears in three files and a header
/// that disagreed with a pointer about where the workspace is would be worse
/// than either being wrong on its own.
fn as_text(path: &Path) -> String {
    path.display().to_string()
}

/// Writes one file, saying which one when it cannot.
async fn write(path: &Path, contents: &str) -> Result<(), WorkspaceError> {
    tokio::fs::write(path, contents)
        .await
        .map_err(|error| WorkspaceError::Write {
            path: path.to_owned(),
            source: error,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const THREAD: &str = "019fd32f-a25f-7611-a4fe-c93cc2a6d782";

    /// The workspace a thread would own inside the container.
    fn fixture_workspace() -> PathBuf {
        PathBuf::from("/workspace").join(THREAD)
    }

    fn assembled(prompt: &str) -> String {
        let settings = ThreadSettings {
            prompt: prompt.to_owned(),
            ..ThreadSettings::default()
        };

        assemble(&layers(&fixture_workspace(), THREAD, &settings))
    }

    #[test]
    fn the_header_states_where_the_agent_is_and_what_it_is_working_in() {
        let file = assembled("");

        assert!(file.contains(&as_text(&fixture_workspace())));
        assert!(file.contains(THREAD));
        assert!(file.contains("repos/"), "{file}");
    }

    #[test]
    fn the_header_comes_first_and_the_prompt_below_it() {
        // Precedence is the whole contract of this file: the satellite's facts
        // are not overridable, and an operator's prompt that landed above them
        // would be read as overriding them.
        let file = assembled("Ship the parser and nothing else.");

        let header = file.find("# Arsox").expect("the header should be present");
        let prompt = file
            .find("Ship the parser")
            .expect("the prompt should be present");

        assert!(header < prompt, "{file}");
    }

    #[test]
    fn a_thread_with_no_prompt_gets_the_header_alone() {
        let file = assembled("");

        assert!(file.starts_with("# Arsox"));
        assert!(file.ends_with("this thread.\n"), "{file}");
    }

    #[test]
    fn a_prompt_that_is_only_whitespace_is_not_a_layer() {
        // Absent and blank are the same instruction, and a trailing blank
        // section reads as instruction that failed to render.
        assert_eq!(assembled("   \n\n\t"), assembled(""));
    }

    #[test]
    fn layers_are_separated_by_exactly_one_blank_line() {
        let file = assembled("\n\nDo the work.\n\n\n");

        assert!(file.contains("this thread.\n\nDo the work.\n"), "{file}");
        assert!(!file.contains("\n\n\n"), "{file}");
    }

    #[test]
    fn the_pointer_imports_the_assembled_file_by_its_real_path() {
        let workspace = fixture_workspace();
        let pointer = POINTER.replace("{workspace}", &as_text(&workspace));

        assert!(pointer.contains(&format!("@{}/AGENTS.md", as_text(&workspace))));
    }
}
