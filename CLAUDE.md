# **Source-of-truth policy.**

The USER is the primary source of truth. This `CLAUDE.md` is the SECOND source of truth. The codebase is **not** a source of truth (it can drift). Whenever a design decision is made or changed, update this file in the same change. Keep it routinely up to date.

# Style

No em dashes anywhere in user-facing text (docs, UI, commits, PRs).

# Arsox

@./README.md

This repo is open-source, public.
The main branch is production (stable), the develop branch is the working branch (unstable) that PRs push into.
If you're a Stakeholder (such as `navarrotech`) then you may push commits straight to develop.
Else you will be required to make a pull request for all other commits.

Stakeholders are responsible for promoting develop to main.

## Protobuf

Protobuf is expected to be compiled into strongly typed dist files for code usage in rust/python/typescript.
I do NOT want runtime imports of a protobuf file and interpreted at runtime, I want it statically compiled and strongly typed with robust third party libraries.
For example, there's a javascript & typescript package that converts protobuf files into dist .js and .d.ts dists.
This dramatically increases the contract of protobuf being the full source of truth and well maintained + documented, fully compiled + checked at build time.
I also am fine with these protobuf dists being checked into git.

## Turn inputs, closing steps, and sessions

- **Turn attachments** name files the host uploaded into the workspace. The bytes decide what a file is, never its name. Claude receives images and PDFs as content blocks in one stream-json message on stdin, and only a turn with such a block switches to stdin; Codex receives images with `-i`. Everything else is named in the prompt. Files are checked at submission and read again when the harness starts. See `docs/harness.md#attachments-reach-the-harness-the-way-it-takes-them`.
- **Turn end hooks** run after the work and before the artifact scan, as the agent, through `supervise.rs`, and never fail the turn. A cancelled turn runs none. See `docs/harness.md#turn-end-hooks`.
- **Harness sessions** leave and enter threads as one tar archive. The satellite sets `CLAUDE_CONFIG_DIR` and `CODEX_HOME` under the agent's home on every launch, so it decides where sessions are rather than guessing. See `docs/harness.md#a-session-can-leave-its-thread`.
