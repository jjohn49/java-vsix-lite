// Styled diagnostic hovers: a hover section that re-renders this extension's
// diagnostics (native tier + javac) with severity color, code-styled type
// names, and a clickable "declared here" link — instead of the editor's
// plain-text diagnostic block. Pure presentation over the diagnostics
// already in `vscode.languages.getDiagnostics`; no server involvement.
//
// Colors use `var(--vscode-*)` theme variables (the only styling VS Code's
// hover sanitizer allows on `<span>`), so they track the active theme.

import * as vscode from "vscode";

/** Severity presentation: codicon, label, and theme color variable. */
const SEVERITY_STYLE: Record<
  vscode.DiagnosticSeverity,
  { icon: string; label: string; color: string }
> = {
  [vscode.DiagnosticSeverity.Error]: {
    icon: "$(error)",
    label: "Error",
    color: "var(--vscode-editorError-foreground)",
  },
  [vscode.DiagnosticSeverity.Warning]: {
    icon: "$(warning)",
    label: "Warning",
    color: "var(--vscode-editorWarning-foreground)",
  },
  [vscode.DiagnosticSeverity.Information]: {
    icon: "$(info)",
    label: "Info",
    color: "var(--vscode-editorInfo-foreground)",
  },
  [vscode.DiagnosticSeverity.Hint]: {
    icon: "$(lightbulb)",
    label: "Hint",
    color: "var(--vscode-editorHint-foreground)",
  },
};

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

/**
 * The diagnostic message with its type/symbol names set in code style: the
 * `incompatible types: X cannot be converted to Y` shape gets both types
 * wrapped, and any `'quoted'` name (unused/unresolved messages) likewise.
 */
function renderMessage(message: string): string {
  const incompatible = /^(incompatible types: )(.+)( cannot be converted to )(.+)$/.exec(message);
  if (incompatible) {
    return (
      escapeHtml(incompatible[1]) +
      `<code>${escapeHtml(incompatible[2])}</code>` +
      escapeHtml(incompatible[3]) +
      `<code>${escapeHtml(incompatible[4])}</code>`
    );
  }
  return escapeHtml(message).replace(/'([^']+)'/g, "<code>$1</code>");
}

/** `file:///...#L5,9` — VS Code jumps to the location from a hover link. */
function locationLink(location: vscode.Location): string {
  const start = location.range.start;
  return `${location.uri.toString()}#L${start.line + 1},${start.character + 1}`;
}

function diagnosticCode(diagnostic: vscode.Diagnostic): string | undefined {
  if (typeof diagnostic.code === "string" || typeof diagnostic.code === "number") {
    return String(diagnostic.code);
  }
  if (diagnostic.code && typeof diagnostic.code === "object") {
    return String(diagnostic.code.value);
  }
  return undefined;
}

/** Whether the styled hover is on (`diagnostics.styledHover`, default true). */
export function styledHoverEnabled(): boolean {
  return vscode.workspace
    .getConfiguration("java-vsix-lite")
    .get<boolean>("diagnostics.styledHover", true);
}

/** Related information relocated out of published diagnostics, keyed by
 * document uri (see `relocateRelatedInformation`). */
const relocated = new Map<
  string,
  { range: vscode.Range; code: string | undefined; related: vscode.DiagnosticRelatedInformation[] }[]
>();

/**
 * Strip `relatedInformation` from this extension's diagnostics before they
 * are published, stashing it for the styled hover. The editor's built-in
 * hover renders every related entry as an extra plain `file(line, col): …`
 * row that would duplicate the styled block's link — with the styled hover
 * on, the plain section shrinks to the one message line VS Code always
 * renders, and the link lives only in the styled block. Called from the
 * language client's `handleDiagnostics` middleware; a no-op (stash cleared)
 * when the styled hover is disabled, so the plain experience keeps its
 * related rows.
 */
export function relocateRelatedInformation(
  uri: vscode.Uri,
  diagnostics: vscode.Diagnostic[],
): void {
  const key = uri.toString();
  if (!styledHoverEnabled()) {
    relocated.delete(key);
    return;
  }
  const stash: {
    range: vscode.Range;
    code: string | undefined;
    related: vscode.DiagnosticRelatedInformation[];
  }[] = [];
  for (const diagnostic of diagnostics) {
    if (diagnostic.relatedInformation && diagnostic.relatedInformation.length > 0) {
      stash.push({
        range: diagnostic.range,
        code: diagnosticCode(diagnostic),
        related: diagnostic.relatedInformation,
      });
      diagnostic.relatedInformation = undefined;
    }
  }
  if (stash.length > 0) {
    relocated.set(key, stash);
  } else {
    relocated.delete(key);
  }
}

/** The stashed related entries for one published diagnostic, if any. */
function relatedFor(
  uri: vscode.Uri,
  diagnostic: vscode.Diagnostic,
): vscode.DiagnosticRelatedInformation[] {
  if (diagnostic.relatedInformation && diagnostic.relatedInformation.length > 0) {
    return diagnostic.relatedInformation;
  }
  const code = diagnosticCode(diagnostic);
  return (
    relocated
      .get(uri.toString())
      ?.find((entry) => entry.range.isEqual(diagnostic.range) && entry.code === code)?.related ??
    []
  );
}

/** One diagnostic as a styled markdown block. Exported for the Electron
 * suite, which asserts on the rendered hover content. */
export function renderDiagnostic(
  diagnostic: vscode.Diagnostic,
  related: vscode.DiagnosticRelatedInformation[],
): string {
  const style =
    SEVERITY_STYLE[diagnostic.severity] ?? SEVERITY_STYLE[vscode.DiagnosticSeverity.Error];
  const badge = [diagnostic.source, diagnosticCode(diagnostic)].filter(Boolean).join(" · ");
  const lines = [
    `<span style="color:${style.color};">${style.icon} **${style.label}**</span>` +
      (badge.length > 0
        ? ` — <span style="color:var(--vscode-descriptionForeground);">${escapeHtml(badge)}</span>`
        : ""),
    "",
    renderMessage(diagnostic.message),
  ];
  for (const entry of related) {
    lines.push("", `$(go-to-file) [${escapeHtml(entry.message)}](${locationLink(entry.location)})`);
  }
  return lines.join("\n");
}

class StyledDiagnosticHoverProvider implements vscode.HoverProvider {
  provideHover(
    document: vscode.TextDocument,
    position: vscode.Position,
  ): vscode.Hover | undefined {
    if (!styledHoverEnabled()) {
      return undefined;
    }
    const hits = vscode.languages
      .getDiagnostics(document.uri)
      .filter(
        (d) =>
          d.range.contains(position) &&
          (d.source === "java-vsix-lite" || d.source === "javac"),
      );
    if (hits.length === 0) {
      return undefined;
    }
    const markdown = new vscode.MarkdownString(
      hits.map((hit) => renderDiagnostic(hit, relatedFor(document.uri, hit))).join("\n\n---\n\n"),
      true,
    );
    markdown.supportHtml = true;
    // Trusted only for the location links rendered above — every URI comes
    // from `DiagnosticRelatedInformation.location`, never from
    // workspace-controlled text.
    markdown.isTrusted = true;
    return new vscode.Hover(markdown, hits[0].range);
  }
}

/** Register the styled hover for Java documents (real files and the
 * read-only `jvl-src` virtual documents). */
export function activateStyledHover(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.languages.registerHoverProvider(
      [
        { scheme: "file", language: "java" },
        { scheme: "jvl-src", language: "java" },
      ],
      new StyledDiagnosticHoverProvider(),
    ),
  );
}
