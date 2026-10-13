> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Pi Inbox in this fork

Select lines in a saved local file and use **Inbox: Add Thread** in the command
palette or context menu, or drag the gutter **+** to select lines. Post Markdown,
read agent replies inline, edit your own messages, and resolve/reopen discussions.
Use **Inbox: Show Threads**, **Inbox: Toggle Resolved**, and **Inbox: Refresh** to
browse and manage the shared inbox. Enter posts or saves an edit; Shift+Enter
adds a newline. Escape cancels a draft or edit.

Posting saves immediately; run `/check-inbox` in pi to request an agent reply.
The database defaults to `~/.pi/agent/data/pi-inbox/messages.sqlite`; set
`pi_inbox.agent_dir` in user settings or `PI_CODING_AGENT_DIR` to share another pi
agent directory. `PI_INBOX_DATABASE` overrides the database path. Restart Zed after
changing the directory or environment variables. `pi_inbox.enabled` disables the
feature; `pi_inbox.show_status_button` hides its status button.
Legacy schemas are rejected, never reset. Unmatched anchors remain file-level
discussions. Remote projects, historical/deleted-side comments, and cross-file
selections are not supported.

This fork depends on the shared Rust core `inbx` from
[pi-inbox](https://github.com/fu5ha/pi-inbox), pinned by git revision in the
workspace `Cargo.toml`. To develop against a local pi-inbox checkout instead,
add a paths override pointing at it (e.g. in a local-only change to
`.cargo/config.toml`): `paths = ["../pi-inbox/crates/inbx"]`. To integrate
a new pi-inbox revision, push pi-inbox, bump the pinned `rev` in the
workspace `Cargo.toml`, and re-lock
(`cargo update -w`).

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
