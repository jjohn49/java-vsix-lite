//! Javadoc → Markdown rendering, so hover and completion docs show
//! real sections instead of raw `@param`/`@return` tag soup.
//!
//! Input is the margin-stripped comment text `signature::strip_javadoc`
//! produces (no `/** */`, no leading `*`); output is Markdown for the
//! `MarkupContent` hover/completion surfaces. Handles the common Javadoc
//! vocabulary — block tags, inline tags, and the HTML subset real-world
//! docs (incl. the JDK's) actually use — and deliberately leaves anything
//! it doesn't recognize verbatim: an unknown `<AngleBracket>` construct is
//! more likely prose (`List<String>` outside `{@code}`) than markup, and
//! showing a stray tag beats eating text.

/// Render margin-stripped Javadoc text as Markdown.
pub(crate) fn render_markdown(cleaned: &str) -> String {
    // Protect `<pre>` blocks (fenced code) from every later transform.
    let (text, fences) = extract_pre_blocks(cleaned);

    let (description, tags) = split_block_tags(&text);

    let mut out = String::new();
    // `@deprecated` leads, matching the standard doclet's rendering order.
    for tag in tags.iter().filter(|t| t.name == "deprecated") {
        push_paragraph(
            &mut out,
            &format!("**Deprecated.** {}", inline_markdown(&tag.rest)),
        );
    }
    push_paragraph(&mut out, &inline_markdown(&description));

    let params: Vec<&BlockTag> = tags.iter().filter(|t| t.name == "param").collect();
    if !params.is_empty() {
        let mut section = String::from("**Parameters:**");
        for tag in params {
            let (name, desc) = split_first_word(&tag.rest);
            section.push_str(&format!("\n- `{}`", name));
            if !desc.is_empty() {
                section.push_str(&format!(" — {}", inline_markdown(desc)));
            }
        }
        push_paragraph(&mut out, &section);
    }

    for tag in tags.iter().filter(|t| t.name == "return") {
        push_paragraph(
            &mut out,
            &format!("**Returns:** {}", inline_markdown(&tag.rest)),
        );
    }

    let throws: Vec<&BlockTag> = tags
        .iter()
        .filter(|t| t.name == "throws" || t.name == "exception")
        .collect();
    if !throws.is_empty() {
        let mut section = String::from("**Throws:**");
        for tag in throws {
            let (ty, desc) = split_first_word(&tag.rest);
            section.push_str(&format!("\n- `{}`", ty));
            if !desc.is_empty() {
                section.push_str(&format!(" — {}", inline_markdown(desc)));
            }
        }
        push_paragraph(&mut out, &section);
    }

    for tag in &tags {
        let label = match tag.name.as_str() {
            "apiNote" => "API Note:",
            "implSpec" => "Implementation Requirements:",
            "implNote" => "Implementation Note:",
            _ => continue,
        };
        push_paragraph(
            &mut out,
            &format!("**{label}** {}", inline_markdown(&tag.rest)),
        );
    }

    for tag in tags.iter().filter(|t| t.name == "since") {
        push_paragraph(
            &mut out,
            &format!("**Since:** {}", inline_markdown(&tag.rest)),
        );
    }

    let sees: Vec<&BlockTag> = tags.iter().filter(|t| t.name == "see").collect();
    if !sees.is_empty() {
        let mut section = String::from("**See also:**");
        for tag in sees {
            section.push_str(&format!("\n- {}", see_reference(&tag.rest)));
        }
        push_paragraph(&mut out, &section);
    }

    restore_pre_blocks(out.trim().to_string(), &fences)
}

/// One `@tag rest…` block (rest includes continuation lines).
struct BlockTag {
    name: String,
    rest: String,
}

/// Tags rendered by a dedicated section above; anything else (`@author`,
/// `@version`, `@serial`, custom tags like the JDK's `@jls`) is dropped —
/// they're metadata the standard doclet also hides or that adds noise to a
/// hover card.
const RENDERED_TAGS: &[&str] = &[
    "param",
    "return",
    "throws",
    "exception",
    "deprecated",
    "since",
    "see",
    "apiNote",
    "implSpec",
    "implNote",
];

/// Split the text into the leading description and its `@tag` blocks. A tag
/// starts a line (after optional whitespace) with `@word`; following lines
/// that don't start a new tag are its continuation. `{@code}` braces never
/// start a line as `@word`, so inline tags are unaffected.
fn split_block_tags(text: &str) -> (String, Vec<BlockTag>) {
    let mut description = String::new();
    let mut tags: Vec<BlockTag> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let tag_name = trimmed.strip_prefix('@').and_then(|rest| {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            (!name.is_empty()).then_some(name)
        });
        if let Some(name) = tag_name {
            let rest = trimmed[1 + name.len()..].trim_start().to_string();
            tags.push(BlockTag { name, rest });
        } else if let Some(current) = tags.last_mut() {
            if !current.rest.is_empty() {
                current.rest.push('\n');
            }
            current.rest.push_str(trimmed);
        } else {
            if !description.is_empty() {
                description.push('\n');
            }
            description.push_str(line);
        }
    }
    tags.retain(|t| RENDERED_TAGS.contains(&t.name.as_str()));
    (description, tags)
}

fn push_paragraph(out: &mut String, paragraph: &str) {
    let trimmed = paragraph.trim();
    // A section whose content rendered to nothing (e.g. a bare `@return`)
    // still shows its label — bold-only fragments like `**Returns:**` are
    // fine; a fully empty paragraph is not.
    if trimmed.is_empty() || trimmed == "**Deprecated.**" {
        if trimmed == "**Deprecated.**" {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(trimmed);
        }
        return;
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(trimmed);
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(at) => (&s[..at], s[at..].trim_start()),
        None => (s, ""),
    }
}

/// A `@see` target: `{@link}`-shaped and `<a href>` forms go through the
/// inline pipeline; a bare code reference (`java.util.List#add`) is
/// code-quoted; a quoted string stays as-is.
fn see_reference(rest: &str) -> String {
    let rest = rest.trim();
    if rest.starts_with('{') || rest.starts_with('<') || rest.starts_with('"') {
        inline_markdown(rest)
    } else {
        format!("`{rest}`")
    }
}

// --- inline tags + HTML subset ---

/// Transform inline Javadoc tags and the common HTML subset to Markdown.
/// Inline-tag renderings (`{@code}`, `{@literal}`, `{@link}` output) are
/// final-form Markdown and must not be re-processed by the HTML/entity
/// passes — `{@literal a<b>}` means a literal `a<b>`, not bold — so they
/// ride through as placeholders and are restored at the end.
fn inline_markdown(text: &str) -> String {
    let (text, protected) = inline_tags(text);
    let text = anchors(&text);
    let text = html_subset(&text);
    let mut text = entities(&text);
    for (i, chunk) in protected.iter().enumerate() {
        text = text.replace(&format!("\u{fffb}{i}\u{fffb}"), chunk);
    }
    text
}

/// `{@tag …}` constructs, brace-balanced (code samples contain `{}`) —
/// each rendering is emitted as a `\u{fffb}i\u{fffb}` placeholder with the
/// real text in the returned vec (see [`inline_markdown`]).
fn inline_tags(text: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut protected = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{@") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some((body, len)) = balanced_braces(after) else {
            out.push_str(after);
            return (out, protected);
        };
        out.push_str(&format!("\u{fffb}{}\u{fffb}", protected.len()));
        protected.push(render_inline_tag(body));
        rest = &after[len..];
    }
    out.push_str(rest);
    (out, protected)
}

/// The content between the outermost braces of a `{…}` starting at byte 0,
/// plus the total length consumed (including both braces). `None` when
/// unbalanced — the caller leaves the text verbatim.
fn balanced_braces(s: &str) -> Option<(&str, usize)> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[1..i], i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

/// The Markdown for one inline tag body (`@code x`, `@link A#b label`, …).
fn render_inline_tag(body: &str) -> String {
    let (tag, rest) = split_first_word(body);
    match tag {
        "@code" => {
            let code = rest.trim();
            if code.is_empty() {
                String::new()
            } else if code.contains('\n') {
                format!("```\n{code}\n```")
            } else {
                format!("`{code}`")
            }
        }
        "@literal" => rest.trim().to_string(),
        "@link" | "@linkplain" | "@value" => {
            let (target, label) = split_first_word(rest);
            let display = if label.is_empty() {
                link_display(target)
            } else {
                label.trim().to_string()
            };
            if tag == "@linkplain" {
                display
            } else {
                format!("`{display}`")
            }
        }
        "@inheritDoc" => String::new(),
        // Unknown inline tag: keep its text, drop the tag syntax.
        _ => rest.trim().to_string(),
    }
}

/// A `{@link}` target with no explicit label, made readable:
/// `java.util.List#add(Object)` → `List.add(Object)`, `java.util.List` →
/// `List`, `#size()` → `size()`, `Outer.Inner` → `Outer.Inner`. Heuristic:
/// after normalizing `#` to `.`, keep the last segment, plus the one before
/// it when that looks like a type (starts uppercase) — packages are
/// lowercase by convention, types aren't.
fn link_display(target: &str) -> String {
    let target = target.strip_prefix('#').unwrap_or(target).replace('#', ".");
    let segments: Vec<&str> = target.split('.').collect();
    match segments.as_slice() {
        [.., before_last, last]
            if before_last
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase()) =>
        {
            format!("{before_last}.{last}")
        }
        [.., last] => (*last).to_string(),
        [] => target.clone(),
    }
}

/// `<a href="U">label</a>` → `[label](U)` — its own pass, before
/// [`html_subset`], because an anchor spans from open tag through close tag
/// (unlike the single-tag substitutions there). A malformed anchor (no
/// parsable `href`, no close tag) stays verbatim.
fn anchors(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::new();
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find("<a ") {
        let start = pos + rel;
        let parsed = (|| {
            let tag_end = start + text[start..].find('>')?;
            let tag = &text[start + 1..tag_end];
            let (_, tail) = tag
                .split_once("href=\"")
                .or_else(|| tag.split_once("href='"))?;
            let quote = if tag.contains("href=\"") { '"' } else { '\'' };
            let (url, _) = tail.split_once(quote)?;
            let close_rel = lower[tag_end..].find("</a>")?;
            let label = text[tag_end + 1..tag_end + close_rel].trim();
            Some((
                format!("[{label}]({url})"),
                tag_end + close_rel + "</a>".len(),
            ))
        })();
        match parsed {
            Some((rendered, consumed_to)) => {
                out.push_str(&text[pos..start]);
                out.push_str(&rendered);
                pos = consumed_to;
            }
            None => {
                out.push_str(&text[pos..start + 3]);
                pos = start + 3;
            }
        }
    }
    out.push_str(&text[pos..]);
    out
}

/// Known HTML → Markdown. Only tags on this fixed list are touched;
/// anything else in angle brackets stays verbatim (see module docs).
fn html_subset(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find('>') else {
            out.push_str(after);
            return out;
        };
        let tag = &after[1..end];
        let lower = tag.trim_end_matches('/').trim().to_ascii_lowercase();
        let replacement: Option<&str> = match lower.as_str() {
            "p" | "/p" => Some("\n\n"),
            "br" => Some("\n"),
            "code" | "/code" | "tt" | "/tt" => Some("`"),
            "b" | "/b" | "strong" | "/strong" => Some("**"),
            "i" | "/i" | "em" | "/em" => Some("*"),
            "ul" | "/ul" | "ol" | "/ol" | "/li" => Some("\n"),
            "li" => Some("\n- "),
            "/a" => Some(""), // stray close tag (its open tag didn't parse)
            _ => None,
        };
        match replacement {
            Some(r) => out.push_str(r),
            None => out.push_str(&after[..end + 1]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

// --- <pre> fences ---

/// Replace each `<pre>…</pre>` block with a placeholder, returning the
/// fenced-code Markdown to restore afterward. `<pre>{@code …}</pre>` (the
/// dominant real-world shape) unwraps the inline tag so the fence contains
/// only the code.
fn extract_pre_blocks(text: &str) -> (String, Vec<String>) {
    let mut fences = Vec::new();
    let mut out = String::new();
    let mut rest = text;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = lower.find("<pre>") else {
            out.push_str(rest);
            break;
        };
        let Some(close) = lower[start..].find("</pre>") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let inner = &rest[start + "<pre>".len()..start + close];
        let code = pre_content(inner);
        out.push_str(&format!("\u{fffc}{}\u{fffc}", fences.len()));
        fences.push(format!("```java\n{}\n```", code.trim_matches('\n')));
        rest = &rest[start + close + "</pre>".len()..];
    }
    (out, fences)
}

/// The code inside a `<pre>` block: unwrap a `{@code …}` wrapper and decode
/// entities (pre blocks in older docs escape `<`/`&` by hand).
fn pre_content(inner: &str) -> String {
    let trimmed = inner.trim();
    let unwrapped = if trimmed.starts_with("{@code") {
        match balanced_braces(trimmed) {
            Some((body, len)) if trimmed[len..].trim().is_empty() => {
                body.strip_prefix("@code").unwrap_or(body).to_string()
            }
            _ => trimmed.to_string(),
        }
    } else {
        trimmed.to_string()
    };
    entities(&unwrapped)
}

fn restore_pre_blocks(mut text: String, fences: &[String]) -> String {
    for (i, fence) in fences.iter().enumerate() {
        let placeholder = format!("\u{fffc}{i}\u{fffc}");
        // A fence swallowed into a paragraph still needs blank lines around
        // it to render as a block.
        text = text.replace(&placeholder, &format!("\n\n{fence}\n\n"));
    }
    // Collapse any 3+ newline runs the restoration introduced.
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(render_markdown("The width."), "The width.");
        assert_eq!(
            render_markdown("Line one.\nLine two."),
            "Line one.\nLine two."
        );
    }

    #[test]
    fn param_return_throws_render_as_sections() {
        let src = "Greets a person warmly.\n\
                   \n\
                   @param name who to greet\n\
                   @param <T> the greeting type\n\
                   @return the greeting text\n\
                   @throws IllegalArgumentException if name is empty";
        assert_eq!(
            render_markdown(src),
            "Greets a person warmly.\n\n\
             **Parameters:**\n\
             - `name` — who to greet\n\
             - `<T>` — the greeting type\n\n\
             **Returns:** the greeting text\n\n\
             **Throws:**\n\
             - `IllegalArgumentException` — if name is empty"
        );
    }

    #[test]
    fn tag_continuation_lines_attach_to_their_tag() {
        let src = "Does things.\n\
                   @param name a name that\n\
                   spans two lines\n\
                   @return done";
        let out = render_markdown(src);
        assert!(
            out.contains("- `name` — a name that\nspans two lines"),
            "{out}"
        );
        assert!(out.contains("**Returns:** done"), "{out}");
    }

    #[test]
    fn deprecated_leads_the_output() {
        let src = "Old thing.\n@deprecated use {@link NewThing} instead";
        assert_eq!(
            render_markdown(src),
            "**Deprecated.** use `NewThing` instead\n\nOld thing."
        );
    }

    #[test]
    fn inline_code_literal_and_links() {
        assert_eq!(
            render_markdown("Returns {@code null} on miss."),
            "Returns `null` on miss."
        );
        assert_eq!(
            render_markdown("Use {@literal a<b>} here."),
            "Use a<b> here."
        );
        assert_eq!(render_markdown("See {@link String}."), "See `String`.");
        assert_eq!(
            render_markdown("See {@link java.util.List#add(Object)}."),
            "See `List.add(Object)`."
        );
        assert_eq!(
            render_markdown("See {@link java.util.List#add(Object) the add method}."),
            "See `the add method`."
        );
        assert_eq!(
            render_markdown("or {@linkplain java.util.Map#get get} it"),
            "or get it"
        );
        assert_eq!(
            render_markdown("A {@code Map<K, {V}>} map."),
            "A `Map<K, {V}>` map."
        );
    }

    #[test]
    fn inherit_doc_and_unknown_inline_tags_degrade_gracefully() {
        assert_eq!(render_markdown("{@inheritDoc}"), "");
        assert_eq!(render_markdown("per {@jls 3.10} rules"), "per 3.10 rules");
    }

    #[test]
    fn html_subset_renders_as_markdown() {
        assert_eq!(render_markdown("First.<p>Second."), "First.\n\nSecond.");
        assert_eq!(render_markdown("a<br>b"), "a\nb");
        assert_eq!(
            render_markdown("<b>bold</b> and <i>italic</i>"),
            "**bold** and *italic*"
        );
        assert_eq!(render_markdown("<code>x + y</code>"), "`x + y`");
        assert_eq!(
            render_markdown("Kinds:\n<ul><li>alpha</li><li>beta</li></ul>"),
            "Kinds:\n\n- alpha\n\n- beta"
        );
        assert_eq!(
            render_markdown("3 &lt; 4 &amp;&amp; a &gt; b"),
            "3 < 4 && a > b"
        );
        // Unknown angle constructs are prose, not markup — untouched.
        assert_eq!(
            render_markdown("a List<String> of names"),
            "a List<String> of names"
        );
    }

    #[test]
    fn anchors_become_markdown_links() {
        assert_eq!(
            render_markdown(r#"See <a href="https://example.com/spec">the spec</a>."#),
            "See [the spec](https://example.com/spec)."
        );
    }

    #[test]
    fn pre_code_blocks_become_fences() {
        let src = "Example:\n<pre>{@code\nint x = 1;\nfoo(x);\n}</pre>\nDone.";
        assert_eq!(
            render_markdown(src),
            "Example:\n\n```java\nint x = 1;\nfoo(x);\n```\n\nDone."
        );
        // Plain <pre> without {@code}, with hand-escaped entities.
        let src = "<pre>\nList&lt;String&gt; xs;\n</pre>";
        assert_eq!(render_markdown(src), "```java\nList<String> xs;\n```");
    }

    #[test]
    fn pre_content_is_protected_from_other_transforms() {
        // `@param`-looking lines and `<b>` inside a fence must stay literal.
        let src = "<pre>{@code\n@param not a tag\na < b\n}</pre>";
        let out = render_markdown(src);
        assert!(out.contains("@param not a tag"), "{out}");
        assert!(out.contains("a < b"), "{out}");
        assert!(!out.contains("**Parameters:**"), "{out}");
    }

    #[test]
    fn since_see_and_dropped_metadata_tags() {
        let src = "Thing.\n\
                   @author A. Hacker\n\
                   @version 1.2\n\
                   @since 1.8\n\
                   @see java.util.List#add(Object)\n\
                   @see \"The Java Language Specification\"";
        assert_eq!(
            render_markdown(src),
            "Thing.\n\n\
             **Since:** 1.8\n\n\
             **See also:**\n\
             - `java.util.List#add(Object)`\n\
             - \"The Java Language Specification\""
        );
    }

    #[test]
    fn api_note_and_impl_spec_sections() {
        let src = "Core doc.\n@implSpec The default implementation returns {@code null}.";
        assert_eq!(
            render_markdown(src),
            "Core doc.\n\n**Implementation Requirements:** The default implementation returns `null`."
        );
    }

    #[test]
    fn bare_tags_render_labels_without_panic() {
        let out = render_markdown("Doc.\n@return\n@param x\n@deprecated");
        assert!(out.contains("**Returns:**"), "{out}");
        assert!(out.contains("- `x`"), "{out}");
        assert!(out.starts_with("**Deprecated.**"), "{out}");
    }
}
