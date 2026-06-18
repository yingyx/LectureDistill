//! LaTeX conversion and PDF rendering module.
//!
//! Converts distilled Markdown into a compact Chinese-capable A4 cheat-sheet
//! PDF. Prefers Typst when available and falls back to xelatex, latexmk,
//! tectonic, or pdflatex.

use crate::artifacts::CheatSheetArtifact;
use anyhow::{bail, Context, Result};
use regex::Regex;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Embedded default LaTeX template
// ---------------------------------------------------------------------------

/// Embedded default LaTeX template (fallback when no template file is found).
/// A compact four-column portrait A4 cheat sheet template.
pub const DEFAULT_TEMPLATE: &str = r#"% !TEX program = xelatex
\documentclass[a4paper]{article}
\usepackage{iftex}
\ifPDFTeX
  \errmessage{The default lecture-distill cheat-sheet template requires XeLaTeX or latexmk -xelatex for Chinese text}
\fi
\usepackage[no-math]{fontspec}
\usepackage{xeCJK}
\usepackage[a4paper,left=0.5mm,right=0.5mm,top=0.5mm,bottom=0.5mm,noheadfoot]{geometry}
\usepackage{xcolor}
\usepackage{multicol}
\usepackage{titlesec}
\usepackage{enumitem}
\usepackage{amsmath,amssymb,mathtools}
\usepackage{array,booktabs,tabularx}

\IfFontExistsTF{Arial}{\setmainfont{Arial}}{\setmainfont{Latin Modern Roman}}
\IfFontExistsTF{Microsoft YaHei}{\setCJKmainfont{Microsoft YaHei}}{%
  \IfFontExistsTF{SimSun}{\setCJKmainfont{SimSun}}{\setCJKmainfont{Noto Sans CJK SC}}%
}
\defaultfontfeatures{Ligatures=TeX}
\xeCJKsetup{CJKmath=true,CheckSingle=true,PunctStyle=kaiming}

\definecolor{CheatTitleBlue}{HTML}{004D80}
\definecolor{CheatAccentBlue}{HTML}{0076BA}
\definecolor{CheatDividerGray}{HTML}{808080}

\setlength{\columnsep}{2mm}
\setlength{\columnseprule}{0pt}
\setlength{\parindent}{0pt}
\setlength{\parskip}{0pt}
\setlength{\topskip}{0pt}
\pagestyle{empty}
\raggedright
\emergencystretch=1em

\makeatletter
\renewcommand\normalsize{\@setfontsize\normalsize{5pt}{6pt}}
\makeatother
\normalsize

\titleformat{\section}{\color{CheatTitleBlue}\fontsize{7pt}{7.5pt}\selectfont\bfseries}{}{0pt}{}
\titleformat{\subsection}{\color{CheatTitleBlue}\fontsize{6pt}{6.5pt}\selectfont\bfseries}{}{0pt}{}
\titleformat{\subsubsection}{\color{CheatAccentBlue}\fontsize{5.5pt}{6pt}\selectfont\bfseries}{}{0pt}{}
\titleformat{\paragraph}[runin]{\color{CheatAccentBlue}\fontsize{5pt}{6pt}\selectfont\bfseries}{}{0pt}{}
\titlespacing*{\section}{0pt}{1.2pt}{0.6pt}
\titlespacing*{\subsection}{0pt}{0.9pt}{0.4pt}
\titlespacing*{\subsubsection}{0pt}{0.6pt}{0.3pt}
\titlespacing*{\paragraph}{0pt}{0.5pt}{0.4em}
\setcounter{secnumdepth}{0}

\setlist[itemize]{leftmargin=0.85em,label={-},labelsep=0.25em,itemsep=0pt,topsep=0pt,parsep=0pt,partopsep=0pt}
\setlist[enumerate]{leftmargin=1.1em,labelsep=0.25em,itemsep=0pt,topsep=0pt,parsep=0pt,partopsep=0pt}
\setlength{\abovedisplayskip}{1pt plus 0.5pt minus 0.5pt}
\setlength{\belowdisplayskip}{1pt plus 0.5pt minus 0.5pt}
\setlength{\abovedisplayshortskip}{0.5pt plus 0.5pt}
\setlength{\belowdisplayshortskip}{0.5pt plus 0.5pt}
\allowdisplaybreaks

\newcommand{\divider}{\par\vspace{1pt}{\color{CheatDividerGray}\hrule height 0.4pt width \linewidth}\vspace{1pt}}
\newenvironment{cheatsheet}{\begin{multicols*}{4}\raggedcolumns\normalsize}{\end{multicols*}}

\begin{document}
\begin{cheatsheet}
{{content}}
\end{cheatsheet}
\end{document}
"#;

/// Embedded default Typst template (preferred PDF renderer).
pub const DEFAULT_TYPST_TEMPLATE: &str = r##"#set page(
  paper: "a4",
  margin: (x: 0.5mm, y: 0.5mm),
)
#set text(
  font: ("Microsoft YaHei", "SimSun", "Noto Sans CJK SC", "Arial", "New Computer Modern"),
  size: 5pt,
  lang: "zh",
)
#set par(
  leading: 0pt,
  spacing: 0pt,
  first-line-indent: 0pt,
  justify: false,
)
#show heading: set block(above: 0.8pt, below: 0.4pt)
#show heading.where(level: 1): set text(fill: rgb("#004D80"), size: 7pt, weight: "bold")
#show heading.where(level: 2): set text(fill: rgb("#004D80"), size: 6pt, weight: "bold")
#show heading.where(level: 3): set text(fill: rgb("#0076BA"), size: 5.5pt, weight: "bold")
#show list: set block(spacing: 0pt)
#show enum: set block(spacing: 0pt)

#columns(4, gutter: 2mm)[
{{content}}
]
"##;

// ---------------------------------------------------------------------------
// Compiler discovery
// ---------------------------------------------------------------------------

/// Find an available LaTeX compiler on the system PATH.
///
/// Checks in order: `xelatex`, `latexmk`, `tectonic`, `pdflatex`.
/// Returns the compiler name, or empty string if none found.
pub fn find_latex_compiler() -> String {
    let candidates: &[&str] = if cfg!(windows) {
        &["xelatex.exe", "latexmk.exe", "tectonic.exe", "pdflatex.exe"]
    } else {
        &["xelatex", "latexmk", "tectonic", "pdflatex"]
    };

    for name in candidates {
        // Try running <name> --version to confirm it exists and is runnable.
        match Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {
                // Strip .exe for uniformity on Windows.
                return name.trim_end_matches(".exe").to_string();
            }
            _ => {}
        }
    }

    // On Windows also try without .exe (some installations omit it).
    #[cfg(windows)]
    {
        for name in &["xelatex", "latexmk", "tectonic", "pdflatex"] {
            match Command::new(name)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
            {
                Ok(status) if status.success() => return name.to_string(),
                _ => {}
            }
        }
    }

    String::new()
}

/// Return the configured Typst executable from `LECTURE_DISTILL_TYPST_PATH`.
fn configured_typst_path() -> Option<String> {
    std::env::var("LECTURE_DISTILL_TYPST_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Check whether a Typst executable can be invoked.
fn is_typst_runnable(exe: &str) -> bool {
    Command::new(exe)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Find Typst, preferring the configured executable path and falling back to PATH.
pub fn find_typst_compiler() -> String {
    if let Some(path) = configured_typst_path() {
        if is_typst_runnable(&path) {
            return path;
        }
    }

    let candidates: &[&str] = if cfg!(windows) {
        &["typst.exe", "typst"]
    } else {
        &["typst"]
    };

    for name in candidates {
        if is_typst_runnable(name) {
            return name.to_string();
        }
    }

    String::new()
}

/// Human-readable renderer status for UI/API surfaces.
pub fn find_pdf_renderer() -> String {
    let typst = find_typst_compiler();
    if !typst.is_empty() {
        return format!("typst ({})", typst);
    }

    let latex = find_latex_compiler();
    if !latex.is_empty() {
        return format!("latex ({})", latex);
    }

    String::new()
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

fn path_for_tex(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Compile a `.tex` file to PDF using the given compiler.
///
/// * `xelatex`: runs XeLaTeX twice for stable output
/// * `tectonic`: copies `.tex` to `output_dir`, runs `tectonic -Z search=false <dest>`
/// * `latexmk`: runs `latexmk -xelatex -interaction=nonstopmode -outdir=<dir> <tex_path>`
/// * `pdflatex`: runs `pdflatex -interaction=nonstopmode -output-directory=<dir> <tex_path>`
///
/// Returns the path to the generated PDF file.
pub fn compile_latex(tex_path: &str, output_dir: &str, compiler: &str) -> Result<String> {
    let tex_path = Path::new(tex_path);
    let output_dir = Path::new(output_dir);

    fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "failed to create output directory: {}",
            output_dir.display()
        )
    })?;

    let stem = tex_path
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
        .to_string_lossy()
        .to_string();

    match compiler {
        "xelatex" => {
            let run_once = || {
                Command::new("xelatex")
                    .args([
                        "-interaction=nonstopmode",
                        "-halt-on-error",
                        &format!("-output-directory={}", path_for_tex(output_dir)),
                        &path_for_tex(tex_path),
                    ])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run xelatex")
            };
            let status = run_once()?;
            if status.success() {
                let _ = run_once();
            }
            let pdf_path = output_dir.join(format!("{}.pdf", stem));
            if !pdf_path.exists() {
                bail!(
                    "xelatex did not produce a PDF file (exit status: {})",
                    status
                );
            }
            if !status.success() {
                log::warn!(
                    "xelatex exited with non-zero status {} but PDF was produced",
                    status
                );
            }
        }
        "tectonic" => {
            // Tectonic compiles in-place, so copy the .tex file into the output
            // directory first.
            let dest_tex = output_dir.join(format!("{}.tex", stem));
            fs::copy(tex_path, &dest_tex).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    tex_path.display(),
                    dest_tex.display()
                )
            })?;

            let status = Command::new("tectonic")
                .args(["-Z", "search=false", &path_for_tex(&dest_tex)])
                .current_dir(output_dir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .status()
                .context("failed to run tectonic")?;

            if !status.success() {
                bail!("tectonic exited with non-zero status: {}", status);
            }
        }
        "latexmk" => {
            let status = Command::new("latexmk")
                .args([
                    "-xelatex",
                    "-interaction=nonstopmode",
                    "-halt-on-error",
                    &format!("-outdir={}", path_for_tex(output_dir)),
                    &path_for_tex(tex_path),
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .status()
                .context("failed to run latexmk")?;

            // latexmk may return non-zero even on success; check for the PDF.
            let pdf_path = output_dir.join(format!("{}.pdf", stem));
            if !pdf_path.exists() {
                bail!(
                    "latexmk did not produce a PDF file (exit status: {})",
                    status
                );
            }
            if !status.success() {
                log::warn!(
                    "latexmk exited with non-zero status {} but PDF was produced",
                    status
                );
            }
        }
        "pdflatex" => {
            let status = Command::new("pdflatex")
                .args([
                    "-interaction=nonstopmode",
                    &format!("-output-directory={}", path_for_tex(output_dir)),
                    &path_for_tex(tex_path),
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .status()
                .context("failed to run pdflatex")?;

            // pdflatex may need a second pass for cross-references; run it once
            // more for safety.
            if status.success() {
                let _ = Command::new("pdflatex")
                    .args([
                        "-interaction=nonstopmode",
                        &format!("-output-directory={}", path_for_tex(output_dir)),
                        &path_for_tex(tex_path),
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }

            let pdf_path = output_dir.join(format!("{}.pdf", stem));
            if !pdf_path.exists() {
                bail!("pdflatex did not produce a PDF file");
            }
        }
        other => bail!("unknown LaTeX compiler: {}", other),
    }

    let pdf_path = output_dir.join(format!("{}.pdf", stem));
    Ok(pdf_path.to_string_lossy().to_string())
}

/// Compile a `.typ` file to PDF with the Typst CLI.
pub fn compile_typst(
    typ_path: &str,
    output_pdf_path: &str,
    typst_compiler: &str,
) -> Result<String> {
    let typ_path = Path::new(typ_path);
    let output_pdf_path = Path::new(output_pdf_path);

    if let Some(parent) = output_pdf_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory: {}", parent.display()))?;
    }

    let status = Command::new(typst_compiler)
        .args([
            "compile",
            &typ_path.to_string_lossy(),
            &output_pdf_path.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .with_context(|| format!("failed to run typst: {}", typst_compiler))?;

    if !output_pdf_path.exists() {
        bail!("typst did not produce a PDF file (exit status: {})", status);
    }

    if !status.success() {
        log::warn!(
            "typst exited with non-zero status {} but PDF was produced",
            status
        );
    }

    Ok(output_pdf_path.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// PDF utilities
// ---------------------------------------------------------------------------

/// Count pages in a PDF file using `lopdf`.
pub fn count_pdf_pages(pdf_path: &str) -> Result<usize> {
    let doc = lopdf::Document::load(pdf_path)
        .with_context(|| format!("failed to open PDF: {}", pdf_path))?;
    Ok(doc.get_pages().len())
}

// ---------------------------------------------------------------------------
// Content compression
// ---------------------------------------------------------------------------

/// Compress markdown content for smaller LaTeX output.
///
/// Attempt 1: Remove duplicate blank lines, compact whitespace.
/// Attempt 2: Remove "## Supporting Concepts" section via regex.
/// Attempt 3: Keep only "## Key Concepts" and "## Content Summary" sections.
/// Fallback: truncate to first 50% of content.
pub fn compress_content(content: &str, attempt: usize) -> String {
    match attempt {
        1 => {
            // Remove duplicate blank lines (collapse runs of \n to at most two).
            let re_blank = Regex::new(r"\n{3,}").unwrap();
            let compressed = re_blank.replace_all(content, "\n\n").to_string();
            // Trim trailing whitespace from each line.
            compressed
                .lines()
                .map(|l| l.trim_end())
                .collect::<Vec<&str>>()
                .join("\n")
        }
        2 => {
            // Keep all h2 sections but truncate each body to ~65 % of original length.
            // This preserves topic coverage while reducing total content.
            let result = truncate_all_h2_sections(content, 2, 3);
            // Also remove "Supporting Concepts" h2 sections when present (Ref Digest).
            let result = remove_h2_sections(&result, &["Supporting Concepts"]);
            let re_blank = Regex::new(r"\n{3,}").unwrap();
            re_blank.replace_all(&result, "\n\n").to_string()
        }
        3 => {
            // Aggressive: keep all h2 sections but truncate each body to ~40 %.
            // Every topic survives — no hard character cutoff.
            truncate_all_h2_sections(content, 2, 5)
        }
        _ => content.to_string(), // attempt 0 or unknown - no compression
    }
}

fn h2_heading_matches(line: &str, prefixes: &[&str]) -> bool {
    let Some(rest) = line.strip_prefix("## ") else {
        return false;
    };
    prefixes.iter().any(|prefix| rest.starts_with(prefix))
}

fn remove_h2_sections(content: &str, prefixes: &[&str]) -> String {
    let mut out = Vec::new();
    let mut skipping = false;

    for line in content.lines() {
        if line.starts_with("## ") {
            skipping = h2_heading_matches(line, prefixes);
        }
        if !skipping {
            out.push(line);
        }
    }

    out.join("\n")
}

fn take_char_fraction(content: &str, numerator: usize, denominator: usize) -> String {
    if denominator == 0 || content.is_empty() {
        return String::new();
    }
    let char_count = content.chars().count();
    let take_count = (char_count.saturating_mul(numerator) / denominator).max(1);
    content.chars().take(take_count).collect()
}

/// Truncate every h2 section body proportionally so all topics survive.
///
/// Splits at `\n## ` boundaries, keeps preamble and all section headings intact,
/// and truncates each section body to `fraction_numer / fraction_denom` of its
/// original length (minimum 200 chars per section).  If no h2 headings exist,
/// falls back to global `take_char_fraction`.
fn truncate_all_h2_sections(content: &str, fraction_numer: usize, fraction_denom: usize) -> String {
    // Find the first "\n## " that starts a real h2 section.
    let first_h2 = match content.find("\n## ") {
        Some(pos) => pos,
        None => return take_char_fraction(content, fraction_numer, fraction_denom),
    };

    let preamble = &content[..first_h2];
    let mut result = String::with_capacity(content.len());
    result.push_str(preamble.trim_end());
    result.push('\n');

    // Split remaining content on "\n## " to get individual sections.
    let body = &content[first_h2 + 1..]; // skip the leading \n
    let sections: Vec<&str> = body.split("\n## ").collect();

    let total_section_chars: usize = sections.iter().map(|s| s.chars().count()).sum();
    if total_section_chars == 0 {
        return result;
    }

    // Compute total char budget for all section bodies.
    let total_budget = (total_section_chars.saturating_mul(fraction_numer) / fraction_denom)
        .max(sections.len().saturating_mul(200));

    for section in &sections {
        let heading_end = section.find('\n').unwrap_or(section.len());
        let heading = &section[..heading_end];
        let body_start = if heading_end < section.len() {
            heading_end + 1
        } else {
            heading_end
        };
        let body_text = &section[body_start..];
        let body_chars = body_text.chars().count();

        // Proportional allocation with a floor of 200 chars.
        let alloc = if total_section_chars > 0 {
            ((body_chars as u64 * total_budget as u64) / total_section_chars as u64)
                .min(usize::MAX as u64) as usize
        } else {
            200
        }
        .max(200);

        result.push_str("## ");
        result.push_str(heading);
        result.push('\n');
        if body_chars > 0 && alloc > 0 {
            let truncated: String = body_text.chars().take(alloc).collect();
            result.push_str(truncated.trim_end());
        }
        result.push_str("\n\n");
    }

    // Collapse consecutive blank lines.
    let re_blank = Regex::new(r"\n{3,}").unwrap();
    re_blank.replace_all(result.trim_end(), "\n\n").to_string()
}

// ---------------------------------------------------------------------------
// Markdown -> LaTeX conversion
// ---------------------------------------------------------------------------

/// Escape special LaTeX characters in text.
///
/// Skips if text contains `$` (math mode) or existing `\command` patterns.
/// Replaces: `&` `%` `#` `_` `{` `}` `~` `^` with LaTeX-safe equivalents.
/// Also handles backslash: `\` -> `\textbackslash{}`.
pub fn escape_latex(text: &str) -> String {
    // If the text contains math mode delimiters, return as-is to avoid breaking
    // math expressions.
    if text.contains('$') {
        return text.to_string();
    }

    // If the text already looks like a LaTeX command generated by this module,
    // return as-is.
    if text.trim_start().starts_with('\\')
        || text.contains(r"\textbf{")
        || text.contains(r"\textit{")
        || text.contains(r"\texttt{")
    {
        return text.to_string();
    }

    let mut escaped = String::with_capacity(text.len() + 16);
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str(r"\&"),
            '%' => escaped.push_str(r"\%"),
            '#' => escaped.push_str(r"\#"),
            '_' => escaped.push_str(r"\_"),
            '{' => escaped.push_str(r"\{"),
            '}' => escaped.push_str(r"\}"),
            '~' => escaped.push_str(r"\textasciitilde{}"),
            '^' => escaped.push_str(r"\^{}"),
            '\\' => escaped.push_str(r"\textbackslash{}"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn escape_latex_plain(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 16);
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str(r"\&"),
            '%' => escaped.push_str(r"\%"),
            '#' => escaped.push_str(r"\#"),
            '_' => escaped.push_str(r"\_"),
            '{' => escaped.push_str(r"\{"),
            '}' => escaped.push_str(r"\}"),
            '~' => escaped.push_str(r"\textasciitilde{}"),
            '^' => escaped.push_str(r"\^{}"),
            '\\' => escaped.push_str(r"\textbackslash{}"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Apply inline formatting conversions to a line of text.
///
/// Handles: bold (`**...**` -> `\textbf{...}`), italic (`*...*` -> `\textit{...}`),
/// and inline code (`` `...` `` -> `\texttt{...}`).
fn convert_inline_formatting(text: &str) -> String {
    let text = text.to_string();

    // Bold: **...** -> \textbf{...}
    let re_bold = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    let text = re_bold.replace_all(&text, r"\textbf{$1}").to_string();

    // Inline code: `...` -> \texttt{...}
    let re_code = Regex::new(r"`([^`]+)`").unwrap();
    let text = re_code.replace_all(&text, r"\texttt{$1}").to_string();

    convert_italic_markers(&text)
}

fn markdown_inline_to_latex(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut parts = text.split('$').peekable();
    let mut in_math = false;

    while let Some(part) = parts.next() {
        if in_math {
            out.push('$');
            out.push_str(part);
            if parts.peek().is_some() {
                out.push('$');
            }
        } else {
            let escaped = escape_latex_plain(part);
            out.push_str(&convert_inline_formatting(&escaped));
        }
        in_math = !in_math;
    }

    out
}

fn convert_italic_markers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '*' {
            let prev_is_star = i > 0 && chars[i - 1] == '*';
            let next_is_star = i + 1 < chars.len() && chars[i + 1] == '*';
            if !prev_is_star && !next_is_star {
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j] == '*' {
                        let before_is_star = j > 0 && chars[j - 1] == '*';
                        let after_is_star = j + 1 < chars.len() && chars[j + 1] == '*';
                        if !before_is_star && !after_is_star {
                            break;
                        }
                    }
                    j += 1;
                }
                if j < chars.len() {
                    let inner: String = chars[i + 1..j].iter().collect();
                    out.push_str(r"\textit{");
                    out.push_str(&inner);
                    out.push('}');
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Convert Markdown content to LaTeX body text.
///
/// Line-by-line converter:
/// - Math blocks (`$$`, `\[`) passed through
/// - HTML comments (`<!--`) and blockquotes (`>`) skipped
/// - Headings: `#` -> `\section*{}`, `##` -> `\subsection*{}`,
///   `###` -> `\subsubsection*{}`, `####+` -> `\paragraph*{}`
/// - Unordered lists (`-` / `*`) -> `\begin{itemize}` / `\end{itemize}`
/// - Ordered lists (`1.` / `1)`) -> `\begin{enumerate}` / `\end{enumerate}`
/// - Lists close on blank lines
/// - Bold (`**...**`) -> `\textbf{...}`
/// - Italic (`*...*`) -> `\textit{...}`
/// - Inline code (`` `...` ``) -> `\texttt{...}`
/// - Other lines: escaped via `escape_latex`
pub fn markdown_to_latex(md_content: &str) -> String {
    let mut output = String::with_capacity(md_content.len() + 1024);

    // List state machine.
    enum ListState {
        None,
        Itemize,
        Enumerate,
    }
    let mut list_state = ListState::None;

    // Track whether we are inside a display math block ($$ or \[...\]).
    let mut in_math_block = false;
    let mut math_block_delim = "";

    // Regexes (compiled once per call - cheap enough for this use case).
    let re_heading = Regex::new(r"^(#{1,6})\s+(.+)$").unwrap();
    let re_ulist = Regex::new(r"^[\-\*]\s+(.+)$").unwrap();
    let re_olist_dot = Regex::new(r"^\d+\.\s+(.+)$").unwrap();
    let re_olist_paren = Regex::new(r"^\d+\)\s+(.+)$").unwrap();
    let re_comment = Regex::new(r"^\s*<!--").unwrap();
    let re_blockquote = Regex::new(r"^>\s?").unwrap();
    let re_math_open = Regex::new(r"^\$\$").unwrap();
    let re_math_close = Regex::new(r"\$\$$").unwrap();
    let re_dmath_open = Regex::new(r"^\\\[").unwrap();
    let re_dmath_close = Regex::new(r"\\\]$").unwrap();

    // Helper to close any open list environment.
    let close_list = |state: &mut ListState, out: &mut String| match state {
        ListState::Itemize => {
            out.push_str("\\end{itemize}\n");
            *state = ListState::None;
        }
        ListState::Enumerate => {
            out.push_str("\\end{enumerate}\n");
            *state = ListState::None;
        }
        ListState::None => {}
    };

    for line in md_content.lines() {
        // -- Display math block handling ----------------------------------
        if !in_math_block {
            if re_math_open.is_match(line) {
                in_math_block = true;
                math_block_delim = "$$";
                output.push_str(line);
                output.push('\n');
                continue;
            }
            if re_dmath_open.is_match(line) {
                in_math_block = true;
                math_block_delim = "\\[";
                output.push_str(line);
                output.push('\n');
                continue;
            }
        } else {
            output.push_str(line);
            output.push('\n');
            if math_block_delim == "$$" && re_math_close.is_match(line) {
                in_math_block = false;
                math_block_delim = "";
            }
            if math_block_delim == "\\[" && re_dmath_close.is_match(line) {
                in_math_block = false;
                math_block_delim = "";
            }
            continue;
        }

        // -- Blank line - close any open list ----------------------------
        if line.trim().is_empty() {
            close_list(&mut list_state, &mut output);
            output.push('\n');
            continue;
        }

        // -- HTML comments -----------------------------------------------
        if re_comment.is_match(line) {
            continue;
        }

        // -- Blockquotes -------------------------------------------------
        if re_blockquote.is_match(line) {
            continue;
        }

        // -- Headings ----------------------------------------------------
        if let Some(caps) = re_heading.captures(line) {
            close_list(&mut list_state, &mut output);
            let level = caps.get(1).unwrap().as_str().len();
            let text = caps.get(2).unwrap().as_str().trim();
            let formatted = markdown_inline_to_latex(text);
            let latex_cmd = match level {
                1 => format!("\\section*{{{}}}\n", formatted),
                2 => format!("\\subsection*{{{}}}\n", formatted),
                3 => format!("\\subsubsection*{{{}}}\n", formatted),
                _ => format!("\\paragraph*{{{}}}\n", formatted),
            };
            output.push_str(&latex_cmd);
            continue;
        }

        // -- Unordered list items ----------------------------------------
        if let Some(caps) = re_ulist.captures(line) {
            match list_state {
                ListState::Itemize => {} // already open
                ListState::Enumerate => {
                    close_list(&mut list_state, &mut output);
                    output.push_str("\\begin{itemize}\n");
                    list_state = ListState::Itemize;
                }
                ListState::None => {
                    output.push_str("\\begin{itemize}\n");
                    list_state = ListState::Itemize;
                }
            }
            let item_text = caps.get(1).unwrap().as_str().trim();
            let formatted = markdown_inline_to_latex(item_text);
            output.push_str(&format!("  \\item {}\n", formatted));
            continue;
        }

        // -- Ordered list items (with dot: "1. text") --------------------
        if let Some(caps) = re_olist_dot.captures(line) {
            match list_state {
                ListState::Enumerate => {} // already open
                ListState::Itemize => {
                    close_list(&mut list_state, &mut output);
                    output.push_str("\\begin{enumerate}\n");
                    list_state = ListState::Enumerate;
                }
                ListState::None => {
                    output.push_str("\\begin{enumerate}\n");
                    list_state = ListState::Enumerate;
                }
            }
            let item_text = caps.get(1).unwrap().as_str().trim();
            let formatted = markdown_inline_to_latex(item_text);
            output.push_str(&format!("  \\item {}\n", formatted));
            continue;
        }

        // -- Ordered list items (with paren: "1) text") ------------------
        if let Some(caps) = re_olist_paren.captures(line) {
            match list_state {
                ListState::Enumerate => {} // already open
                ListState::Itemize => {
                    close_list(&mut list_state, &mut output);
                    output.push_str("\\begin{enumerate}\n");
                    list_state = ListState::Enumerate;
                }
                ListState::None => {
                    output.push_str("\\begin{enumerate}\n");
                    list_state = ListState::Enumerate;
                }
            }
            let item_text = caps.get(1).unwrap().as_str().trim();
            let formatted = markdown_inline_to_latex(item_text);
            output.push_str(&format!("  \\item {}\n", formatted));
            continue;
        }

        // -- Regular text line -------------------------------------------
        // A non-blank, non-special line closes any open list.
        close_list(&mut list_state, &mut output);

        let formatted = markdown_inline_to_latex(line.trim_end());
        output.push_str(&formatted);
        output.push('\n');
    }

    // Close any still-open list.
    close_list(&mut list_state, &mut output);

    // Close any still-open math block (unlikely but safe).
    if in_math_block {
        output.push_str("\\]\n");
    }

    output
}

fn escape_typst_plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' | '#' | '$' | '%' | '&' | '_' | '^' | '~' | '*' | '[' | ']' | '<' | '>' | '@' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn markdown_inline_to_typst(text: &str) -> String {
    let code_re = Regex::new(r"`([^`]+)`").unwrap();
    let mut result = String::new();
    let mut last = 0;

    for mat in code_re.find_iter(text) {
        result.push_str(&markdown_inline_to_typst_no_code(&text[last..mat.start()]));
        let code = mat.as_str().trim_matches('`').replace('`', "\\`");
        result.push_str(&format!("`{}`", code));
        last = mat.end();
    }

    result.push_str(&markdown_inline_to_typst_no_code(&text[last..]));
    result
}

fn markdown_inline_to_typst_no_code(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;

    while let Some(start) = rest.find('$') {
        let (before, after_start) = rest.split_at(start);
        result.push_str(&escape_typst_with_emphasis(before));
        if let Some(end_rel) = after_start[1..].find('$') {
            let end = end_rel + 2;
            result.push_str(&after_start[..end]);
            rest = &after_start[end..];
        } else {
            result.push_str("\\$");
            rest = &after_start[1..];
        }
    }

    result.push_str(&escape_typst_with_emphasis(rest));
    result
}

fn escape_typst_with_emphasis(text: &str) -> String {
    let bold_re = Regex::new(r"\*\*([^*]+)\*\*").unwrap();
    let italic_re = Regex::new(r"(?P<pre>^|[^*])\*([^*]+)\*").unwrap();

    let mut result = String::new();
    let mut last = 0;
    for caps in bold_re.captures_iter(text) {
        let mat = caps.get(0).unwrap();
        result.push_str(&escape_typst_plain(&text[last..mat.start()]));
        result.push('*');
        result.push_str(&escape_typst_plain(&caps[1]));
        result.push('*');
        last = mat.end();
    }
    result.push_str(&escape_typst_plain(&text[last..]));

    let mut converted = String::new();
    let mut last = 0;
    for caps in italic_re.captures_iter(&result) {
        let mat = caps.get(0).unwrap();
        let pre = caps.name("pre").map(|m| m.as_str()).unwrap_or("");
        let body = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        converted.push_str(&result[last..mat.start()]);
        converted.push_str(pre);
        converted.push('_');
        converted.push_str(body);
        converted.push('_');
        last = mat.end();
    }
    converted.push_str(&result[last..]);
    converted
}

/// Convert a small Markdown subset to Typst markup for the cheat-sheet template.
pub fn markdown_to_typst(md_content: &str) -> String {
    let heading_re = Regex::new(r"^(#{1,6})\s+(.*)$").unwrap();
    let unordered_re = Regex::new(r"^\s*[-*]\s+(.*)$").unwrap();
    let ordered_re = Regex::new(r"^\s*\d+[\.)]\s+(.*)$").unwrap();

    let mut output = String::new();
    let mut in_raw_block = false;

    for line in md_content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("```") {
            in_raw_block = !in_raw_block;
            continue;
        }
        if in_raw_block || trimmed.starts_with("<!--") || trimmed.starts_with('>') {
            continue;
        }
        if trimmed.is_empty() {
            output.push('\n');
            continue;
        }
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            output.push_str("#line(length: 100%, stroke: 0.4pt + rgb(\"#808080\"))\n");
            continue;
        }

        if let Some(caps) = heading_re.captures(trimmed) {
            let level = caps.get(1).unwrap().as_str().len().min(6);
            let text = markdown_inline_to_typst(caps.get(2).unwrap().as_str());
            output.push_str(&format!("{} {}\n", "=".repeat(level), text));
            continue;
        }

        if let Some(caps) = unordered_re.captures(trimmed) {
            output.push_str(&format!("- {}\n", markdown_inline_to_typst(&caps[1])));
            continue;
        }

        if let Some(caps) = ordered_re.captures(trimmed) {
            output.push_str(&format!("+ {}\n", markdown_inline_to_typst(&caps[1])));
            continue;
        }

        output.push_str(&markdown_inline_to_typst(trimmed));
        output.push('\n');
    }

    output
}

// ---------------------------------------------------------------------------
// Template helpers
// ---------------------------------------------------------------------------

/// Write the default template to a file (used for bootstrapping).
pub fn write_default_template(path: &str) -> Result<()> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir for {}", path))?;
    }
    let mut f = fs::File::create(path)
        .with_context(|| format!("failed to create template file: {}", path))?;
    f.write_all(DEFAULT_TEMPLATE.as_bytes())
        .with_context(|| format!("failed to write template: {}", path))?;
    Ok(())
}

/// Resolve template content: try file path first, fall back to embedded default.
fn resolve_template(template_path: Option<&str>) -> Result<String> {
    match template_path {
        Some(p) => {
            if Path::new(p).exists() {
                fs::read_to_string(p)
                    .with_context(|| format!("failed to read template file: {}", p))
            } else {
                log::warn!("template file '{}' not found, using embedded default", p);
                Ok(DEFAULT_TEMPLATE.to_string())
            }
        }
        None => Ok(DEFAULT_TEMPLATE.to_string()),
    }
}

/// Resolve a Typst template. Only `.typ` custom templates are used for Typst;
/// LaTeX templates are ignored so Typst can safely fall back to its default.
fn resolve_typst_template(template_path: Option<&str>) -> Result<String> {
    match template_path {
        Some(path) if Path::new(path).extension().and_then(|e| e.to_str()) == Some("typ") => {
            fs::read_to_string(path)
                .with_context(|| format!("failed to read Typst template: {}", path))
        }
        _ => Ok(DEFAULT_TYPST_TEMPLATE.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Main render pipeline
// ---------------------------------------------------------------------------

/// Render a cheat sheet PDF from distilled Markdown.
///
/// 1. Resolve template (use provided path, fallback to embedded [`DEFAULT_TEMPLATE`])
/// 2. Verify `{{content}}` placeholder exists in template
/// 3. Read input markdown, convert to LaTeX via [`markdown_to_latex`]
/// 4. Find compiler (error if none found)
/// 5. Loop up to 4 iterations (attempt 0 + 3 compression attempts):
///    - Apply compression if attempt > 0
///    - Replace `{{content}}` in template with LaTeX body
///    - Write `cheatsheet.tex` in output directory
///    - Compile; catch errors
///    - Count pages; if <= `max_pages`, copy PDF to final path and return
///      [`CheatSheetArtifact`]
/// 6. If all attempts exhausted, return error
pub fn render_cheatsheet(
    input_md_path: &str,
    template_path: Option<&str>,
    output_pdf_path: &str,
    max_pages: usize,
) -> Result<CheatSheetArtifact> {
    // 1. Resolve template
    let template = resolve_template(template_path)?;
    let typst_template = resolve_typst_template(template_path)?;

    // 2. Verify placeholder
    if !template.contains("{{content}}") {
        bail!("LaTeX template does not contain the required {{content}} placeholder");
    }
    if !typst_template.contains("{{content}}") {
        bail!("Typst template does not contain the required {{content}} placeholder");
    }

    // 3. Read and convert markdown
    let md_content = fs::read_to_string(input_md_path)
        .with_context(|| format!("failed to read input markdown: {}", input_md_path))?;

    // 4. Find PDF renderers. Typst is preferred; LaTeX remains the fallback.
    let typst_compiler = find_typst_compiler();
    let compiler = find_latex_compiler();
    if typst_compiler.is_empty() && compiler.is_empty() {
        bail!(
            "No PDF renderer found. Please install Typst or xelatex/texlive, latexmk, or tectonic."
        );
    }
    if !typst_compiler.is_empty() {
        log::info!("using Typst renderer: {}", typst_compiler);
    }
    if !compiler.is_empty() {
        log::info!("available LaTeX fallback compiler: {}", compiler);
    }

    // Determine output directory (parent of the output PDF path).
    let out_pdf = PathBuf::from(output_pdf_path);
    let output_dir = out_pdf
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let tex_name = "cheatsheet.tex";
    let tex_path = output_dir.join(tex_name);
    let typ_name = "cheatsheet.typ";
    let typ_path = output_dir.join(typ_name);
    let intermediate_pdf_path = output_dir.join("cheatsheet.pdf");

    let typst_template_name = template_path
        .filter(|p| Path::new(p).extension().and_then(|e| e.to_str()) == Some("typ"))
        .and_then(|p| {
            Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "default_cheatsheet.typ".to_string());
    let latex_template_name = template_path
        .filter(|p| Path::new(p).extension().and_then(|e| e.to_str()) != Some("typ"))
        .and_then(|p| {
            Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "default_cheatsheet.tex".to_string());

    let mut last_error: Option<String> = None;

    // 5. Iteration loop - up to 4 attempts
    for attempt in 0..=3 {
        let level_label = if attempt == 0 {
            "none".to_string()
        } else {
            attempt.to_string()
        };
        log::info!(
            "render attempt {}/3 (compression level: {})",
            attempt,
            level_label
        );

        // Apply compression if attempt > 0
        let working_content = if attempt > 0 {
            compress_content(&md_content, attempt)
        } else {
            md_content.clone()
        };

        let mut pdf_path: Option<String> = None;
        let mut used_template = "";
        let mut attempt_errors: Vec<String> = Vec::new();

        if !typst_compiler.is_empty() {
            let typst_body = markdown_to_typst(&working_content);
            let filled_template = typst_template.replace("{{content}}", &typst_body);
            match fs::write(&typ_path, &filled_template) {
                Ok(_) => match compile_typst(
                    &typ_path.to_string_lossy(),
                    &intermediate_pdf_path.to_string_lossy(),
                    &typst_compiler,
                ) {
                    Ok(p) => {
                        pdf_path = Some(p);
                        used_template = &typst_template_name;
                    }
                    Err(e) => {
                        let msg = format!("typst compilation failed: {}", e);
                        log::warn!("{}", msg);
                        attempt_errors.push(msg);
                    }
                },
                Err(e) => {
                    let msg = format!("failed to write .typ file: {}", e);
                    log::warn!("{}", msg);
                    attempt_errors.push(msg);
                }
            }
        }

        if pdf_path.is_none() && !compiler.is_empty() {
            // Convert to LaTeX
            let latex_body = markdown_to_latex(&working_content);

            // Replace placeholder in template
            let filled_template = template.replace("{{content}}", &latex_body);

            // Write .tex file
            if let Err(e) = fs::write(&tex_path, &filled_template) {
                let msg = format!("failed to write .tex file: {}", e);
                log::warn!("{}", msg);
                last_error = Some(msg);
                continue;
            }

            // Compile
            match compile_latex(
                &tex_path.to_string_lossy(),
                &output_dir.to_string_lossy(),
                &compiler,
            ) {
                Ok(p) => {
                    pdf_path = Some(p);
                    used_template = &latex_template_name;
                }
                Err(e) => {
                    let msg = format!("latex compilation failed: {}", e);
                    log::warn!("{}", msg);
                    attempt_errors.push(msg);
                    last_error = Some(attempt_errors.join("; "));
                    continue;
                }
            }
        }

        let pdf_path = match pdf_path {
            Some(path) => path,
            None => {
                if attempt_errors.is_empty() {
                    last_error = Some("no PDF renderer produced an output file".to_string());
                } else {
                    last_error = Some(attempt_errors.join("; "));
                }
                continue;
            }
        };

        // Count pages
        let page_count = match count_pdf_pages(&pdf_path) {
            Ok(n) => n,
            Err(e) => {
                let msg = format!("failed to count PDF pages: {}", e);
                log::warn!("{}", msg);
                last_error = Some(msg);
                continue;
            }
        };

        log::info!("compiled PDF: {} pages (attempt {})", page_count, attempt);

        // Check against max_pages
        if page_count <= max_pages {
            // Success - copy PDF to final output path.
            fs::copy(&pdf_path, &out_pdf).with_context(|| {
                format!(
                    "failed to copy PDF from {} to {}",
                    pdf_path,
                    out_pdf.display()
                )
            })?;

            log::info!(
                "cheat sheet rendered successfully: {} ({} pages, {} compression attempts)",
                out_pdf.display(),
                page_count,
                attempt,
            );

            return Ok(CheatSheetArtifact {
                pdf_path: out_pdf.to_string_lossy().to_string(),
                page_count,
                template_used: used_template.to_string(),
                distilled_content_path: input_md_path.to_string(),
                rendered_at: chrono::Utc::now().to_rfc3339(),
                compression_attempts: attempt,
            });
        }

        // Too many pages - log and retry with more compression.
        log::warn!(
            "PDF has {} pages, exceeding max {} (attempt {})",
            page_count,
            max_pages,
            attempt
        );
        last_error = Some(format!(
            "PDF has {} pages, exceeding maximum of {} pages",
            page_count, max_pages
        ));
    }

    // 6. All attempts exhausted.
    bail!(
        "Failed to produce a PDF within {} pages after 4 attempts. Last error: {}",
        max_pages,
        last_error.unwrap_or_else(|| "unknown".to_string())
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- escape_latex ----------------------------------------------------

    #[test]
    fn escape_latex_special_chars() {
        let input = "Price: 50% off - A & B #1 with _underscore_ {braces} ~tilde^caret";
        let escaped = escape_latex(input);
        // LaTeX escape sequences should be present.
        assert!(escaped.contains(r"\&"));
        assert!(escaped.contains(r"\%"));
        assert!(escaped.contains(r"\#"));
        assert!(escaped.contains(r"\_"));
        assert!(escaped.contains(r"\{"));
        assert!(escaped.contains(r"\}"));
        assert!(escaped.contains(r"\textasciitilde{}"));
        assert!(escaped.contains(r"\^{}"));
        // Raw special chars should NOT appear unescaped:
        // - Unescaped & (not preceded by \) - # and _ follow the same logic.
        //   Simple proxy: the string must differ from the original.
        assert_ne!(escaped, input);
    }

    #[test]
    fn escape_latex_preserves_math_mode() {
        let math = "$E = mc^2$ and $a_b$";
        let escaped = escape_latex(math);
        // Math mode text must be returned verbatim.
        assert_eq!(escaped, math);
    }

    #[test]
    fn escape_latex_preserves_latex_commands() {
        let cmd = r"\textbf{bold} and \textit{italic}";
        let escaped = escape_latex(cmd);
        assert_eq!(escaped, cmd);
    }

    #[test]
    fn escape_latex_backslash_handling() {
        let input = r"a\b";
        let escaped = escape_latex(input);
        assert!(escaped.contains(r"\textbackslash{}"));
    }

    // -- markdown_to_latex -----------------------------------------------

    #[test]
    fn markdown_to_latex_headings() {
        let md = "# Title\n## Section\n### Sub\n#### Deep";
        let latex = markdown_to_latex(md);
        assert!(latex.contains(r"\section*{Title}"));
        assert!(latex.contains(r"\subsection*{Section}"));
        assert!(latex.contains(r"\subsubsection*{Sub}"));
        assert!(latex.contains(r"\paragraph*{Deep}"));
    }

    #[test]
    fn markdown_to_latex_unordered_list() {
        let md = "- item one\n- item two\n- item three";
        let latex = markdown_to_latex(md);
        assert!(latex.contains(r"\begin{itemize}"));
        assert!(latex.contains(r"\end{itemize}"));
        assert!(latex.contains(r"\item item one"));
        assert!(latex.contains(r"\item item two"));
        assert!(latex.contains(r"\item item three"));
    }

    #[test]
    fn markdown_to_latex_ordered_list_dot() {
        let md = "1. first\n2. second\n3. third";
        let latex = markdown_to_latex(md);
        assert!(latex.contains(r"\begin{enumerate}"));
        assert!(latex.contains(r"\end{enumerate}"));
        assert!(latex.contains(r"\item first"));
        assert!(latex.contains(r"\item second"));
    }

    #[test]
    fn markdown_to_latex_ordered_list_paren() {
        let md = "1) alpha\n2) beta";
        let latex = markdown_to_latex(md);
        assert!(latex.contains(r"\begin{enumerate}"));
        assert!(latex.contains(r"\item alpha"));
        assert!(latex.contains(r"\item beta"));
    }

    #[test]
    fn markdown_to_latex_list_closes_on_blank_line() {
        let md = "- a\n- b\n\nplain text\n- c";
        let latex = markdown_to_latex(md);
        // Should have two separate itemize blocks.
        let first_close = latex.find(r"\end{itemize}").unwrap();
        let second_begin = latex[first_close..].find(r"\begin{itemize}");
        assert!(
            second_begin.is_some(),
            "expected second itemize block after blank line"
        );
        assert!(latex.contains("plain text"));
    }

    #[test]
    fn markdown_to_latex_switches_list_type() {
        let md = "- bullet\n\n1. numbered";
        let latex = markdown_to_latex(md);
        assert!(latex.contains(r"\begin{itemize}"));
        assert!(latex.contains(r"\end{itemize}"));
        assert!(latex.contains(r"\begin{enumerate}"));
        assert!(latex.contains(r"\end{enumerate}"));
    }

    #[test]
    fn markdown_to_latex_bold_italic() {
        let md = "**bold** and *italic* text";
        let latex = markdown_to_latex(md);
        assert!(latex.contains(r"\textbf{bold}"));
        assert!(latex.contains(r"\textit{italic}"));
    }

    #[test]
    fn markdown_to_latex_inline_code() {
        let md = "use `foo()` function";
        let latex = markdown_to_latex(md);
        assert!(latex.contains(r"\texttt{foo()}"));
    }

    #[test]
    fn markdown_to_latex_html_comments_skipped() {
        let md = "<!-- this is a comment -->\nvisible text";
        let latex = markdown_to_latex(md);
        assert!(!latex.contains("comment"));
        assert!(latex.contains("visible text"));
    }

    #[test]
    fn markdown_to_latex_blockquote_skipped() {
        let md = "> quoted text\nvisible text";
        let latex = markdown_to_latex(md);
        assert!(!latex.contains("quoted text"));
        assert!(latex.contains("visible text"));
    }

    #[test]
    fn markdown_to_latex_math_block_passthrough() {
        let md = "$$\nE = mc^2\n$$\nplain text";
        let latex = markdown_to_latex(md);
        assert!(latex.contains("$$"));
        assert!(latex.contains("E = mc^2"));
        assert!(latex.contains("plain text"));
    }

    // -- compress_content ------------------------------------------------

    #[test]
    fn compress_content_attempt1_removes_blank_lines() {
        let input = "line one\n\n\n\nline two\n\n\nline three";
        let output = compress_content(input, 1);
        // Should collapse runs of 3+ blank lines to 2.
        let triple_newline = output.contains("\n\n\n");
        assert!(!triple_newline, "should not have triple newlines");
        assert!(output.contains("line one"));
        assert!(output.contains("line two"));
        assert!(output.contains("line three"));
    }

    #[test]
    fn compress_content_attempt2_removes_supporting_section() {
        let input = "\
# Title
intro text

## Key Concepts (Must Know)
- concept A
- concept B

## Supporting Concepts (Understand)
- support B
- support C

## Content Summary
content here
";
        let output = compress_content(input, 2);
        // Still removes "Supporting Concepts" sections (Ref Digest optimisation).
        assert!(output.contains("Key Concepts"));
        assert!(!output.contains("Supporting Concepts"));
        assert!(!output.contains("support B"));
        assert!(output.contains("Content Summary"));
        // Other sections survive (proportional truncation preserves all non-dropped h2s).
    }

    #[test]
    fn compress_content_attempt3_preserves_all_h2_sections() {
        // Use a longer input so proportional truncation visibly reduces output.
        let concept_a = "- concept A detail\n".repeat(20);
        let support_b = "- support detail\n".repeat(15);
        let content = "content paragraph.\n".repeat(18);
        let extra = "more content here.\n".repeat(12);
        let input = format!(
            "# Title\nintro text\n\n## Key Concepts (Must Know)\n{}\n## Supporting Concepts (Understand)\n{}\n## Content Summary\n{}\n## Additional Topic\n{}\n",
            concept_a, support_b, content, extra
        );
        let output = compress_content(&input, 3);
        // All h2 sections survive — proportional truncation, no hard cutoff.
        assert!(output.contains("Key Concepts"));
        assert!(output.contains("Supporting Concepts"));
        assert!(output.contains("Content Summary"));
        assert!(output.contains("Additional Topic"));
        // Output should be meaningfully shorter than input (truncated to ~40 %,
        // but 200‑char per‑section floor limits shrinkage for short inputs).
        assert!(
            output.len() <= input.len() * 8 / 10,
            "expected output <= 80 % of input (got {} vs {} chars)",
            output.len(),
            input.len()
        );
    }

    #[test]
    fn compress_content_attempt3_no_h2_headings_falls_back_to_take_char_fraction() {
        // Content without any "\n## " headings → falls back to global proportional truncation.
        let input = "# Title\n\nSome content without expected sections.\nMore content here.";
        let output = compress_content(input, 3);
        // Fallback: take_char_fraction(content, 2, 5) — roughly 40 % of chars.
        assert!(!output.is_empty());
        assert!(output.len() <= input.len());
        assert!(output.contains("Title"));
    }

    #[test]
    fn compress_content_attempt3_is_utf8_safe() {
        let input =
            "# 信号与系统 大作业速查\n\n同步解调误差分析需要考虑相位差、频偏差和低通滤波器带宽。";
        let output = compress_content(input, 3);
        assert!(!output.is_empty());
        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains("信号"));
    }

    // -- find_latex_compiler ---------------------------------------------

    #[test]
    fn find_latex_compiler_returns_something() {
        // May be empty on CI machines without LaTeX installed, but must not panic.
        let compiler = find_latex_compiler();
        // Just assert it returns a valid string (empty or a compiler name).
        let _ = compiler; // may be empty
    }

    // -- resolve_template ------------------------------------------------

    #[test]
    fn resolve_template_returns_default_when_none() {
        let result = resolve_template(None).unwrap();
        assert!(result.contains("{{content}}"));
        assert!(result.contains(r"\documentclass"));
    }

    #[test]
    fn resolve_template_returns_default_when_file_missing() {
        let result = resolve_template(Some("/nonexistent/path/template.tex")).unwrap();
        assert!(result.contains("{{content}}"));
    }

    #[test]
    fn write_default_template_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.tex");
        write_default_template(&path.to_string_lossy()).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("{{content}}"));
        assert!(content.contains(r"\documentclass"));
    }

    #[test]
    fn resolve_template_reads_file_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.tex");
        let custom = r"\documentclass{article}\begin{document}{{content}}\end{document}";
        fs::write(&path, custom).unwrap();
        let result = resolve_template(Some(&path.to_string_lossy())).unwrap();
        assert_eq!(result, custom);
    }

    // -- integration: markdown -> LaTeX round-trip checks ----------------

    #[test]
    fn markdown_to_latex_mixed_list_types() {
        let md = "\
## My Section

- bullet A
- bullet B

Some text

1. step one
2. step two

More text

- bullet C
";
        let latex = markdown_to_latex(md);
        // Should have two itemize blocks and one enumerate block.
        let itemize_count = latex.matches(r"\begin{itemize}").count();
        let enumerate_count = latex.matches(r"\begin{enumerate}").count();
        assert_eq!(itemize_count, 2, "expected two itemize blocks");
        assert_eq!(enumerate_count, 1, "expected one enumerate block");
        assert!(latex.contains(r"\subsection*{My Section}"));
        assert!(latex.contains("Some text"));
        assert!(latex.contains("More text"));
    }

    #[test]
    fn markdown_to_latex_asterisk_list_not_italic() {
        // Lines starting with "* " are list items, not italic text.
        let md = "* list item";
        let latex = markdown_to_latex(md);
        assert!(latex.contains(r"\item list item"));
        assert!(!latex.contains(r"\textit{"));
    }
}
