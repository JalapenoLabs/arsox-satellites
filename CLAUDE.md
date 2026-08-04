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
