# superzed (Zed fork)

> **NOTE (trademark):** "superzed" derives from the Zed name and may carry
> trademark/DMCA risk. A rename is deliberately deferred — do not invest in
> the name.

This is a fork of [Zed](https://zed.dev) that reorients the editor around
agents. Where it diverges from upstream Zed:

- **Sidebar** (`crates/sidebar`): a Workspaces panel (projects with git
  branch and uncommitted `+/-` diff stats) and a Chats panel (agent threads
  with relative age, agent label, and status filters) replace the title bar
  and bottom dock.
- **Multi-workspace windows** (`crates/workspace/src/multi_workspace.rs`):
  one window hosts N workspaces grouped by worktree ("project groups") with
  quick project/thread switching (`NextProject`/`NextThread` and
  cmd/ctrl-click).
- **Agent-first threads** (`crates/agent_ui`): threads are first-class tabs
  with explicit run states — Running, parked-on-human (awaiting
  confirmation/input), Idle — plus process-liveness detection, and a chat
  header showing harness · model · cwd.
- **Persistent terminals**: terminal threads persist across restarts via
  sidebar terminal-thread metadata instead of a terminal dock panel.
- The bottom dock and title bar are removed; panes render as rounded cards.

Upstream README follows.

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

