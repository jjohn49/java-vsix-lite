# java-vsix-lite

Minimal, low-compute Java language support for VS Code — powered by a pure-Rust language server.

## What it is

java-vsix-lite provides core Java editing features without the overhead of a JVM-based toolchain. The language server (`jvl-server`) is written entirely in Rust and starts instantly. It focuses on the features developers use most, with a security-first design that runs safely in untrusted workspaces.

## Features

- **Syntax highlighting and semantic tokens** — accurate Java token colouring driven by the Rust parser
- **Syntax diagnostics** — parse errors and basic structural issues reported inline
- **Document outline** — class, method, and field structure visible in the Outline panel and breadcrumbs
- **Folding ranges** — collapse blocks, comments, and imports
- **Hover with Javadoc** — display attached Javadoc on hover for project symbols and JDK types
- **Completion** — identifier completion including JDK and dependency signatures

## Security posture

- The pure-Rust default tier never executes project code and runs in **untrusted workspaces**.
- The optional `javac` tier and build commands remain disabled until the workspace is trusted.
- The extension installs without a JVM and makes no outbound network requests at runtime.

## Untrusted workspace support

This extension declares `untrustedWorkspaces.supported: "limited"`. Core features (highlighting, semantic tokens, diagnostics, outline, folding, hover, completion) work in untrusted workspaces. Features that require workspace trust (javac tier, build integration) stay disabled until trust is granted.

## License

MIT OR Apache-2.0 — see the [LICENSE](LICENSE) file.
