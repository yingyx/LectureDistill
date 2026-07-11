//! Native Markdown -> Typst conversion backed by pulldown-cmark and MiTeX.

use anyhow::{bail, Context, Result};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use regex::Regex;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

const MAX_FORMULA_CHARS: usize = 16 * 1024;
const MAX_FORMULA_DEPTH: usize = 64;

const MITEX_PRELUDE: &[u8] = include_bytes!("../vendor/mitex/specs/prelude.typ");
const MITEX_STANDARD: &[u8] = include_bytes!("../vendor/mitex/specs/latex/standard.typ");
const MITEX_MOD: &[u8] = include_bytes!("../vendor/mitex/specs/mod.typ");
const MITEX_LICENSE: &[u8] = include_bytes!("../vendor/mitex/LICENSE");
const MITEX_UPSTREAM: &[u8] = include_bytes!("../vendor/mitex/UPSTREAM.md");

/// Typst declarations prepended to generated documents using this converter.
pub const RUNTIME_PRELUDE: &str = r#"#import "_mitex/specs/mod.typ": mitex-scope
#let mitex-eval(code) = eval("$" + code + "$", scope: mitex-scope)
"#;

pub fn runtime_prelude() -> &'static str {
    RUNTIME_PRELUDE
}

/// Materialize the embedded MiTeX scope beside the generated Typst document.
/// Existing files are only rewritten when their bytes differ.
pub fn prepare_runtime(output_dir: &Path) -> Result<()> {
    let root = output_dir.join("_mitex");
    write_if_changed(&root.join("specs/prelude.typ"), MITEX_PRELUDE)?;
    write_if_changed(&root.join("specs/latex/standard.typ"), MITEX_STANDARD)?;
    write_if_changed(&root.join("specs/mod.typ"), MITEX_MOD)?;
    write_if_changed(&root.join("LICENSE"), MITEX_LICENSE)?;
    write_if_changed(&root.join("UPSTREAM.md"), MITEX_UPSTREAM)?;
    Ok(())
}

fn write_if_changed(path: &Path, content: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|existing| {
        existing.len() == content.len()
            && content_hash(&existing) == content_hash(content)
            && existing == content
    }) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create MiTeX runtime directory: {}",
                parent.display()
            )
        })?;
    }
    fs::write(path, content)
        .with_context(|| format!("failed to write MiTeX runtime file: {}", path.display()))
}

fn content_hash(content: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone, Copy)]
enum ListKind {
    Bullet,
    Ordered,
}

/// Convert the Markdown subset emitted by the distillation prompt into Typst.
pub fn markdown_to_typst(markdown: &str) -> Result<String> {
    validate_math_delimiters(markdown)?;
    let markdown = remove_document_title(markdown);
    let options = parser_options();
    let first_heading = Parser::new_ext(&markdown, options)
        .find_map(|event| match event {
            Event::Start(Tag::Heading { level, .. }) => Some(heading_level(level)),
            _ => None,
        })
        .unwrap_or(1);

    let mut output = String::new();
    let mut lists = Vec::<ListKind>::new();
    let mut item_depth = 0usize;
    let mut skipped_depth = 0usize;

    for event in Parser::new_ext(&markdown, options) {
        if skipped_depth > 0 {
            match event {
                Event::Start(_) => skipped_depth += 1,
                Event::End(_) => skipped_depth -= 1,
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::CodeBlock(_))
            | Event::Start(Tag::BlockQuote(_))
            | Event::Start(Tag::Image { .. })
            | Event::Start(Tag::FootnoteDefinition(_))
            | Event::Start(Tag::Table(_)) => skipped_depth = 1,
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if item_depth == 0 {
                    output.push_str("\n\n");
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                let normalized = heading_level(level).saturating_sub(first_heading) + 1;
                output.push_str(&"=".repeat(normalized.clamp(1, 6)));
                output.push(' ');
            }
            Event::End(TagEnd::Heading(_)) => output.push_str("\n\n"),
            Event::Start(Tag::List(start)) => {
                if !lists.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                lists.push(if start.is_some() {
                    ListKind::Ordered
                } else {
                    ListKind::Bullet
                });
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                if lists.is_empty() && !output.ends_with("\n\n") {
                    output.push('\n');
                }
            }
            Event::Start(Tag::Item) => {
                item_depth += 1;
                output.push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                output.push_str(match lists.last().copied().unwrap_or(ListKind::Bullet) {
                    ListKind::Bullet => "- ",
                    ListKind::Ordered => "+ ",
                });
            }
            Event::End(TagEnd::Item) => {
                item_depth = item_depth.saturating_sub(1);
                if !output.ends_with('\n') {
                    output.push('\n');
                }
            }
            Event::Start(Tag::Strong) => output.push_str("#strong["),
            Event::End(TagEnd::Strong) => output.push(']'),
            Event::Start(Tag::Emphasis) => output.push_str("#emph["),
            Event::End(TagEnd::Emphasis) => output.push(']'),
            Event::Start(Tag::Strikethrough) => output.push_str("#strike["),
            Event::End(TagEnd::Strikethrough) => output.push(']'),
            Event::Start(Tag::Link { .. }) | Event::End(TagEnd::Link) => {}
            Event::Text(text) => output.push_str(&escape_typst_text(&text)),
            Event::Code(code) => {
                output.push_str("#raw(\"");
                output.push_str(&escape_typst_string(&code));
                output.push_str("\")");
            }
            Event::InlineMath(math) => write_math(&mut output, &math, false)?,
            Event::DisplayMath(math) => write_math(&mut output, &math, true)?,
            Event::SoftBreak => output.push(' '),
            Event::HardBreak => output.push_str("#linebreak()\n"),
            Event::Rule => {
                output.push_str("\n#line(length: 100%, stroke: 0.4pt + rgb(\"#808080\"))\n\n")
            }
            Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
            Event::TaskListMarker(_) => {}
            Event::Start(_) | Event::End(_) => {}
        }
    }

    Ok(output.trim().to_string())
}

fn parser_options() -> Options {
    Options::ENABLE_MATH
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
}

fn write_math(output: &mut String, formula: &str, block: bool) -> Result<()> {
    validate_formula(formula)?;
    let converted = mitex::convert_math(formula.trim(), None).map_err(|error| {
        anyhow::anyhow!(
            "MiTeX conversion failed for `{}`: {error}",
            excerpt(formula)
        )
    })?;
    validate_converted_math(&converted)?;
    let escaped = escape_typst_string(&converted);
    if block {
        output.push_str("#math.equation(block: true, mitex-eval(\"");
        output.push_str(&escaped);
        output.push_str("\"))\n\n");
    } else {
        output.push_str("#mitex-eval(\"");
        output.push_str(&escaped);
        output.push_str("\")");
    }
    Ok(())
}

fn validate_formula(formula: &str) -> Result<()> {
    let chars = formula.chars().count();
    if chars > MAX_FORMULA_CHARS {
        bail!("formula is too long ({chars} > {MAX_FORMULA_CHARS} characters)");
    }
    for ch in formula.chars() {
        if ch == '\0' || (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')) {
            bail!("formula contains a forbidden control character");
        }
    }

    let mut depth = 0usize;
    let mut escaped = false;
    for ch in formula.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '{' => {
                depth += 1;
                if depth > MAX_FORMULA_DEPTH {
                    bail!("formula nesting exceeds {MAX_FORMULA_DEPTH} levels");
                }
            }
            '}' if depth == 0 => bail!("formula has an unmatched closing brace"),
            '}' => depth -= 1,
            _ => {}
        }
    }
    if depth != 0 {
        bail!("formula has unbalanced braces");
    }

    validate_environments(formula)
}

fn validate_environments(formula: &str) -> Result<()> {
    let environment = Regex::new(r"\\(begin|end)\s*\{([^{}]+)\}").expect("valid regex");
    let mut stack = Vec::<String>::new();
    let mut recognized = 0usize;
    for capture in environment.captures_iter(formula) {
        recognized += 1;
        let kind = &capture[1];
        let name = capture[2].trim().to_string();
        if kind == "begin" {
            stack.push(name);
        } else if stack.pop().as_deref() != Some(name.as_str()) {
            bail!("formula has a mismatched \\end{{{name}}}");
        }
    }
    let mentioned =
        formula.match_indices("\\begin").count() + formula.match_indices("\\end").count();
    if mentioned != recognized {
        bail!("formula has a malformed \\begin or \\end command");
    }
    if let Some(name) = stack.last() {
        bail!("formula is missing \\end{{{name}}}");
    }
    Ok(())
}

fn validate_converted_math(converted: &str) -> Result<()> {
    let lowered = converted.to_ascii_lowercase();
    const FORBIDDEN: [&str; 6] = [
        "#read(",
        "#include(",
        "#import",
        "#eval(",
        "#plugin(",
        "#sys",
    ];
    if let Some(fragment) = FORBIDDEN
        .iter()
        .find(|fragment| lowered.contains(**fragment))
    {
        bail!("MiTeX output contains forbidden Typst code: {fragment}");
    }
    Ok(())
}

fn validate_math_delimiters(markdown: &str) -> Result<()> {
    let mut open_math: Option<(usize, String)> = None;
    let mut fenced = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let bytes = line.as_bytes();
        let mut index = 0usize;
        let mut code_ticks = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'`' && open_math.is_none() {
                let start = index;
                while index < bytes.len() && bytes[index] == b'`' {
                    index += 1;
                }
                let run = index - start;
                code_ticks = if code_ticks == run {
                    0
                } else if code_ticks == 0 {
                    run
                } else {
                    code_ticks
                };
                continue;
            }
            if bytes[index] == b'$' && code_ticks == 0 && !is_escaped(bytes, index) {
                let run = if bytes.get(index + 1) == Some(&b'$') {
                    2
                } else {
                    1
                };
                match open_math.take() {
                    None => open_math = Some((run, String::new())),
                    Some((open, formula)) if open == run => validate_formula(&formula)?,
                    Some((open, _)) => {
                        bail!("mismatched math delimiters: opened with {open}, closed with {run} dollar signs")
                    }
                }
                index += run;
            } else {
                let ch = line[index..]
                    .chars()
                    .next()
                    .expect("valid character boundary");
                if let Some((_, formula)) = &mut open_math {
                    formula.push(ch);
                }
                index += ch.len_utf8();
            }
        }
        if let Some((_, formula)) = &mut open_math {
            formula.push('\n');
        }
    }
    if let Some((run, _)) = open_math {
        bail!("unclosed math delimiter ({run} dollar signs)");
    }
    Ok(())
}

fn is_escaped(bytes: &[u8], index: usize) -> bool {
    let mut slashes = 0usize;
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        slashes += 1;
        cursor -= 1;
    }
    slashes % 2 == 1
}

fn remove_document_title(markdown: &str) -> String {
    let mut removed = false;
    let mut saw_content = false;
    let mut output = String::with_capacity(markdown.len());
    for line in markdown.lines() {
        let trimmed = line.trim();
        if !removed && !saw_content && trimmed.starts_with("# ") && is_document_title(&trimmed[2..])
        {
            removed = true;
            continue;
        }
        if !trimmed.is_empty() {
            saw_content = true;
        }
        output.push_str(line);
        output.push('\n');
    }
    output
}

fn is_document_title(title: &str) -> bool {
    let lowered = title.trim().to_lowercase();
    [
        "reference digest",
        "cheat sheet",
        "cheating sheet",
        "参考摘要",
        "速查",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
}

fn heading_level(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn escape_typst_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '\\' | '#' | '$' | '[' | ']' | '<' | '>' | '@' | '`' | '_' | '*' | '~'
        ) {
            output.push('\\');
        }
        output.push(ch);
    }
    output
}

fn escape_typst_string(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

fn excerpt(formula: &str) -> String {
    let mut excerpt: String = formula.chars().take(80).collect();
    if formula.chars().count() > 80 {
        excerpt.push('…');
    }
    excerpt.replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn drops_document_title_and_normalizes_first_content_heading() {
        let result = markdown_to_typst("# 课程 Cheat Sheet\n\n## Signals\n### Fourier").unwrap();
        assert!(!result.contains("Cheat Sheet"));
        assert!(result.starts_with("= Signals"));
        assert!(result.contains("== Fourier"));
    }

    #[test]
    fn renders_supported_document_elements_and_utf8() {
        let result = markdown_to_typst(
            "# 信号\n- **粗体**和*斜体*，`fft(x)`\n  1. 子项\n\n---\n\n行内 $x_1$。",
        )
        .unwrap();
        assert!(result.contains("= 信号"));
        assert!(result.contains("#strong[粗体]"));
        assert!(result.contains("#emph[斜体]"));
        assert!(result.contains("#raw(\"fft(x)\")"));
        assert!(result.contains("#line("));
        assert!(result.contains("#mitex-eval("));
    }

    #[test]
    fn converts_formula_corpus() {
        let formulas = [
            r"\frac{a}{b}",
            r"\frac 1 T",
            r"\int_{-\infty}^{\infty}f(t)\,dt",
            r"\begin{cases}0,&t<0\\1,&t\ge0\end{cases}",
            r"\begin{bmatrix}a&b\\c&d\end{bmatrix}",
            r"\begin{aligned}x&=a+b\\y&=c+d\end{aligned}",
            r"\sqrt{x^2+y^2}",
            r"\text{Re}(s)>0",
            r"中文+ x_i^2",
            r"\lim_{x\to0}\frac{\sin x}{x}=1",
            r"ae^{-j\omega t}",
        ];
        for formula in formulas {
            let markdown = format!("${formula}$");
            assert!(markdown_to_typst(&markdown).is_ok(), "{formula}");
        }
    }

    #[test]
    fn rejects_unsafe_or_malformed_math() {
        assert!(markdown_to_typst("$\0$").is_err());
        assert!(markdown_to_typst(r"$\frac{a}{b$").is_err());
        assert!(markdown_to_typst(r"$\begin{cases}x\end{matrix}$").is_err());
        assert!(markdown_to_typst(r"$\unknowncommand{x}$").is_err());
        assert!(markdown_to_typst("$x").is_err());
        let long = format!("${}$", "x".repeat(MAX_FORMULA_CHARS + 1));
        assert!(markdown_to_typst(&long).is_err());
    }

    #[test]
    fn escapes_typst_injection_and_ignores_disallowed_blocks() {
        let result = markdown_to_typst(
            "Text #include \"secret.typ\"\n\n`$not_math$`\n\n```typst\n#read(\"secret\")\n```\n\n![alt](secret.png)",
        )
        .unwrap();
        assert!(result.contains(r"\#include"));
        assert!(!result.contains("#read("));
        assert!(!result.contains("secret.png"));
    }

    #[test]
    fn runtime_writes_only_when_content_changes() {
        let temp = tempfile::tempdir().unwrap();
        prepare_runtime(temp.path()).unwrap();
        let path = temp.path().join("_mitex/specs/mod.typ");
        let first = fs::metadata(&path).unwrap().modified().unwrap();
        prepare_runtime(temp.path()).unwrap();
        let second = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(path).unwrap(), MITEX_MOD);
    }

    #[test]
    fn corpus_compiles_with_real_typst_and_default_template() {
        let compiler = crate::latex::find_typst_compiler();
        if compiler.is_empty() {
            eprintln!("Typst is not installed; skipping real compiler test");
            return;
        }

        let markdown = r#"# 信号与系统
## 核心公式
- **卷积**：$y(t)=x(t)*h(t)$
- *变换*：$X(\omega)=\int_{-\infty}^{\infty}x(t)e^{-j\omega t}\,dt$
- 代码：`fft(x)`

---

$$u(t)=\begin{cases}0,&t<0\\1,&t\ge0\end{cases}$$

- $\sqrt{x^2+y^2}$；$\text{Re}(s)>0$
- $\begin{bmatrix}a&b\\c&d\end{bmatrix}$
- $\begin{aligned}x&=a+b\\y&=c+d\end{aligned}$
"#;
        let temp = tempfile::tempdir().unwrap();
        prepare_runtime(temp.path()).unwrap();
        let body = markdown_to_typst(markdown).unwrap();
        let source = format!(
            "{}{}",
            runtime_prelude(),
            crate::latex::DEFAULT_TYPST_TEMPLATE.replace("{{content}}", &body)
        );
        let typ_path = temp.path().join("corpus.typ");
        let pdf_path = temp.path().join("corpus.pdf");
        fs::write(&typ_path, source).unwrap();
        crate::latex::compile_typst(
            &typ_path.to_string_lossy(),
            &pdf_path.to_string_lossy(),
            &compiler,
        )
        .unwrap();
        assert!(pdf_path.exists());

        let query = Command::new(&compiler)
            .args(["query"])
            .arg(&typ_path)
            .args(["metadata", "--field", "value"])
            .output()
            .unwrap();
        assert!(
            query.status.success(),
            "{}",
            String::from_utf8_lossy(&query.stderr)
        );
        assert!(String::from_utf8_lossy(&query.stdout).contains("lecture-distill-end-marker"));
    }

    #[test]
    fn render_pipeline_selects_native_converter() {
        if crate::latex::find_typst_compiler().is_empty() {
            eprintln!("Typst is not installed; skipping render pipeline test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let markdown_path = temp.path().join("input.md");
        let pdf_path = temp.path().join("output.pdf");
        fs::write(
            &markdown_path,
            "# 信号\n\n- $\\begin{cases}0,&t<0\\\\1,&t\\ge0\\end{cases}$",
        )
        .unwrap();
        crate::latex::render_cheatsheet(
            &markdown_path.to_string_lossy(),
            None,
            &pdf_path.to_string_lossy(),
            3,
        )
        .unwrap();
        assert!(pdf_path.exists());
        assert!(temp.path().join("_mitex/specs/mod.typ").exists());
        let generated = fs::read_to_string(temp.path().join("cheatsheet.typ")).unwrap();
        assert!(generated.contains("_mitex/specs/mod.typ"));
        assert!(generated.contains("mitex-eval"));
    }
}
