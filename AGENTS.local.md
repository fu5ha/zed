First read ./AGENTS.md which is the original in the repo.

## Purpose

This is a local forked version of zed. It will be maintained over time and is not planned to be
upstreamed.

The purpose of the fork is to add features I want that are unlikely to be upstreamable. So far that is:
1. Minimal useful jujutsu source control integration

## Source Control

For the purposes of this fork, we use jj for version control. Prefer jj commands over git commands.

## Working in the fork

When making changes, keep in mind we will need to rebase and solve merge conflicts. Make changes in such
a way that they can be as maintainable as possible over time.

Part of this is to see if there is a jj change that already is related to the change you want
to make. If so, work in a new working change on top, then once the changes are done, integrate
them by squashing into the existing change. That way merge conflicts and rebasing become clearer.

## Building

Use `cargo zed-build` to build and `cargo zed-run` to run with the `release-fast` profile.
Keep the release channel unchanged and use the custom update explanation from `.cargo/config.toml`;
minimize environment/flag changes that break build caching.

After changes, install using `script/install-local-windows.ps1`, which uses the same build defaults.
