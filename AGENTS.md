# Repository Guidelines

## Project Structure & Module Organization

- `crates/syntax/`: Java parsing, symbols, diagnostics, completion, navigation, and refactoring logic.
- `crates/classpath/`: JDK, Maven, Gradle, JAR, and generic-signature resolution.
- `crates/server/`: the `jvl-server` LSP executable and lifecycle integration tests.
- `editors/vscode/`: TypeScript client, packaging, Electron tests, fixtures, and extension metadata.
- `docs/`: design notes and the security threat model.

Keep generated `target/`, `dist/`, `out/`, `.vscode-test/`, `server/`, and `.vsix` artifacts out of commits.

## Build, Test, and Development Commands

Run Rust commands from the repository root:

- `cargo build --workspace --locked`: build all crates.
- `cargo test --workspace --locked --offline`: run Rust unit and LSP lifecycle tests without network access.
- `cargo fmt --all -- --check`: verify Rust formatting.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: enforce lint-clean code.

Run extension commands from `editors/vscode` using Node.js 20+:

- `npm ci`: install locked dependencies.
- `npm run check-types && npm run lint && npm run build`: validate and bundle the client.
- `npm test`: run trusted and untrusted workspace suites in VS Code Electron.
- `npm run package:vsix`: build and bundle the native server into a platform VSIX.

## Coding Style & Naming Conventions

Use `rustfmt` defaults: `snake_case` functions/modules and `CamelCase` types. Preserve `#![forbid(unsafe_code)]`; server stdout is reserved for LSP traffic. TypeScript uses two-space indentation, `camelCase` values/functions, `PascalCase` types, strict type checking, and ESLint. Comments should explain invariants or security decisions.

## Testing Guidelines

Add tests with behavior changes. Rust unit tests live beside code or in `*_tests.rs`; protocol cases belong in `crates/server/tests/`. Extension tests use Mocha and `*.test.ts` under `src/test/`. Security-sensitive changes must cover trusted and untrusted workspaces. CI requires tests, formatting, Clippy, npm audit, type checks, and linting to pass.

## Commit & Pull Request Guidelines

History favors concise, imperative subjects such as `Resolve parent POM properties`. Keep commits scoped; reserve `Release vX.Y.Z: ...` for releases. Pull requests should explain the user-visible effect, design or security tradeoffs, and commands run. Link relevant issues and add screenshots for visible UI changes. Update documentation, fixtures, and tests with behavior or configuration changes.

## Security & Configuration Tips

Never execute project build files in the default Rust tier. Keep process paths machine-scoped, gate `javac`, build tools, and downloads on Workspace Trust, and preserve offline/locked dependency checks. Review `docs/THREAT_MODEL.md` before changing filesystem traversal, archive parsing, subprocesses, or network behavior.
