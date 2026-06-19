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
// Space utilization analysis (Typst query)
// ---------------------------------------------------------------------------

use crate::artifacts::{PageUtilizationData, SpaceUtilization};

/// If the overflow ratio is below this threshold, layout tightening is worth
/// attempting before falling back to content compression.
const LAYOUT_TIGHTEN_OVERFLOW_THRESHOLD: f64 = 1.25; // 125%

/// If the last page utilisation is below this threshold when the page count
/// is within the limit, the result is considered under-utilised and may
/// trigger LLM expansion.
const UNDERFLOW_UTILIZATION_THRESHOLD: f64 = 0.75; // 75%

/// A4 page height in mm (used for utilisation calculations).
const A4_PAGE_HEIGHT_MM: f64 = 297.0;

/// A single element extracted from `typst query` JSON output.
#[derive(Debug, Clone)]
struct TypstElement {
    page: usize,
    y_mm: f64,
    /// Element type (e.g. "heading", "par", "list.item") — kept for
    /// diagnostics via Debug.
    #[allow(dead_code)]
    element_type: String,
}

/// Parse a Typst length string (e.g. `"10.2mm"`, `"5pt"`, `"0.5cm"`, `"1in"`)
/// into millimetres.
fn parse_typst_length_to_mm(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Split numeric prefix from unit suffix.
    let (num_str, unit) =
        if let Some(pos) = s.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-') {
            let (n, u) = s.split_at(pos);
            (n, u)
        } else {
            return s.parse::<f64>().ok(); // assume mm if no unit
        };

    let value: f64 = num_str.parse().ok()?;
    match unit {
        "mm" => Some(value),
        "cm" => Some(value * 10.0),
        "pt" => Some(value * 0.3528),
        "in" => Some(value * 25.4),
        _ => {
            // Unknown unit — try as mm anyway.
            log::debug!("unknown typst length unit: {:?} in {:?}", unit, s);
            value
        }
        .into(),
    }
}

/// Run `typst query` on a compiled `.typ` file and return element positions.
///
/// Queries for `heading`, `list.item`, and `par` elements which cover
/// essentially all text content in the default cheat-sheet template.
fn run_typst_query(typ_path: &str, typst_exe: &str) -> Result<Vec<TypstElement>> {
    // Try multiple selectors individually — Typst 0.14+ is picky about comma
    // syntax and only certain element types are locatable.  We try each one
    // and merge the results.
    let selectors: &[&str] = &["figure", "table", "heading", "enum", "list"];

    let mut all_elements = Vec::new();

    for selector in selectors {
        let output = match std::process::Command::new(typst_exe)
            .args(["query", typ_path, selector, "--field", "location"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(o) => o,
            Err(_) => continue,
        };

        if !output.status.success() {
            // Some selectors aren't locatable — skip silently.
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not locatable") || stderr.contains("failed to evaluate") {
                continue;
            }
            // Other errors might indicate real issues — still try next selector.
            log::debug!("typst query '{}' failed: {}", selector, stderr.trim());
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = match serde_json::from_str(&stdout) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let arr = match json.as_array() {
            Some(a) => a,
            None => continue,
        };

        for item in arr {
            let func = item["func"].as_str().unwrap_or("unknown").to_string();
            let loc = &item["location"];
            let page = loc["page"].as_u64().unwrap_or(1) as usize;
            let y_str = loc["y"].as_str().unwrap_or("0mm");

            let y_mm = parse_typst_length_to_mm(y_str).unwrap_or(0.0);

            all_elements.push(TypstElement {
                page,
                y_mm,
                element_type: func,
            });
        }
    }

    if all_elements.is_empty() {
        bail!(
            "typst query returned no locatable elements (selector(s) attempted: {:?}). \
             This is expected with the default template — space utilisation will be \
             estimated from page count and char budget.",
            selectors
        );
    }

    Ok(all_elements)
}

/// Compute per-page space utilisation from `typst query` element positions.
fn compute_space_utilization(
    elements: &[TypstElement],
    page_count: usize,
    max_pages: usize,
) -> SpaceUtilization {
    // Find the maximum page number present.
    let total_pages = elements
        .iter()
        .map(|e| e.page)
        .max()
        .unwrap_or(page_count)
        .max(page_count);

    // Group: for each page find the max y (deepest content).
    let mut page_max_y: Vec<f64> = vec![0.0; total_pages];
    let mut page_has_elements: Vec<bool> = vec![false; total_pages];
    for el in elements {
        let idx = el.page - 1; // 0-based
        if idx < page_max_y.len() {
            page_max_y[idx] = page_max_y[idx].max(el.y_mm);
            page_has_elements[idx] = true;
        }
    }

    // Build per-page utilisation data.
    let mut page_utilizations: Vec<PageUtilizationData> = Vec::with_capacity(total_pages);
    let mut total_content_pages = 0.0_f64;

    for p in 0..total_pages {
        let max_y = if page_has_elements[p] {
            page_max_y[p]
        } else {
            // Page with no elements detected — assume it's fully used
            // (e.g. images, figures, or elements we didn't query).
            // Fall back to a conservative ~95% estimate.
            A4_PAGE_HEIGHT_MM * 0.95
        };
        let util = (max_y / A4_PAGE_HEIGHT_MM * 100.0).min(100.0);
        page_utilizations.push(PageUtilizationData {
            page: p + 1,
            utilization_pct: util,
        });
        total_content_pages += util / 100.0;
    }

    let last_page_utilization_pct = page_utilizations
        .last()
        .map(|p| p.utilization_pct)
        .unwrap_or(100.0);

    let overflow_ratio = if page_count > max_pages {
        Some(total_content_pages / max_pages as f64)
    } else {
        None
    };

    let last_page_under_utilized = page_count <= max_pages
        && last_page_utilization_pct < UNDERFLOW_UTILIZATION_THRESHOLD * 100.0;

    SpaceUtilization {
        total_content_pages,
        max_pages,
        overflow_ratio,
        page_utilizations,
        last_page_under_utilized,
        last_page_utilization_pct,
    }
}

/// Fallback space utilisation estimate when `typst query` is unavailable.
///
/// Uses a char-budget model: each page holds ~11000 chars comfortably.
/// When `input_chars` is provided, the estimate is more accurate; otherwise
/// falls back to coarse page-count heuristics.
fn estimate_space_utilization_from_pages(
    page_count: usize,
    max_pages: usize,
) -> SpaceUtilization {
    let total_content_pages = page_count as f64;
    let overflow_ratio = if page_count > max_pages {
        Some(page_count as f64 / max_pages.max(1) as f64)
    } else {
        None
    };

    // When pages < max_pages, estimate per-page utilisation based on how
    // many pages are actually used vs available.  If only 1 of 2 pages is
    // used, the last page is the only page — estimate ~95% if the render
    // stopped at page_count (no overflow), meaning the single page is full.
    let last_page_util = if page_count > max_pages {
        100.0
    } else if page_count < max_pages {
        // Pages < max: the last (and only) page is likely near-full since
        // the typst/latex renderer didn't need more pages.  Estimate ~95%.
        95.0
    } else {
        // Exact match: assume well-filled.
        95.0
    };

    // Under-utilised when fewer pages were used than allowed.  Even if the
    // existing page(s) are full, the document as a whole has unused capacity
    // — expansion should be attempted to fill the remaining pages.
    let last_page_under = page_count < max_pages;

    let mut page_utils = Vec::with_capacity(page_count.max(1));
    for p in 0..page_count {
        let util = if p + 1 < page_count || page_count == 1 {
            last_page_util
        } else {
            95.0
        };
        page_utils.push(PageUtilizationData {
            page: p + 1,
            utilization_pct: util,
        });
    }

    SpaceUtilization {
        total_content_pages,
        max_pages,
        overflow_ratio,
        page_utilizations: page_utils,
        last_page_under_utilized: last_page_under,
        last_page_utilization_pct: last_page_util,
    }
}

/// Try to run `typst query` and compute space utilisation.
///
/// Returns `None` on any error (missing typst, parse failure, etc.) so that
/// callers can gracefully skip optimisation without breaking the build.
fn try_compute_space_utilization(
    typ_path: &str,
    typst_exe: &str,
    page_count: usize,
    max_pages: usize,
) -> Option<SpaceUtilization> {
    match run_typst_query(typ_path, typst_exe) {
        Ok(elements) => {
            let elem_count = elements.len();
            let su = compute_space_utilization(&elements, page_count, max_pages);
            log::info!(
                "space utilisation: total_content_pages={:.2}, overflow_ratio={:?}, last_page={:.1}%{}",
                su.total_content_pages,
                su.overflow_ratio,
                su.last_page_utilization_pct,
                if su.last_page_under_utilized {
                    " (under-utilised)"
                } else {
                    ""
                }
            );
            // Per-page utilization breakdown.
            for page_data in &su.page_utilizations {
                log::info!(
                    "  page {}: {:.1}% utilised",
                    page_data.page,
                    page_data.utilization_pct
                );
            }
            // Also emit via web_log for CLI/stderr visibility.
            crate::utils::output::web_log(format!(
                "space_util: elements={} pages={} total_content_pages={:.2} overflow_ratio={:?} last_page_util={:.1}% under_utilized={}",
                elem_count,
                su.page_utilizations.len(),
                su.total_content_pages,
                su.overflow_ratio,
                su.last_page_utilization_pct,
                su.last_page_under_utilized,
            ));
            for page_data in &su.page_utilizations {
                crate::utils::output::web_log(format!(
                    "space_util:   page {}: {:.1}% utilised",
                    page_data.page,
                    page_data.utilization_pct,
                ));
            }
            Some(su)
        }
        Err(e) => {
            log::warn!("typst query skipped: {:#}", e);
            crate::utils::output::web_log(format!(
                "space_util: typst query failed — {:#}",
                e,
            ));
            // Fallback: estimate space utilisation from page count and char budget.
            // This is less precise than typst query but still useful for diagnostics.
            let fallback = estimate_space_utilization_from_pages(page_count, max_pages);
            crate::utils::output::web_log(format!(
                "space_util: using fallback estimate — total_content={:.2} last_page={:.1}% under={}",
                fallback.total_content_pages,
                fallback.last_page_utilization_pct,
                fallback.last_page_under_utilized,
            ));
            Some(fallback)
        }
    }
}

/// Generate a tighter variant of the default Typst template by tweaking
/// layout parameters.
///
/// Only called when using the embedded [`DEFAULT_TYPST_TEMPLATE`]; custom user
/// templates are never modified.
///
/// * `level` 1 — mild tightening (gutter 1.2mm, body 4.8pt, tighter heading spacing)
/// * `level` 2 — aggressive tightening (gutter 0.8mm, body 4.5pt, very tight headings)
fn generate_tighter_typst_template(base: &str, level: usize) -> String {
    let mut result = base.to_string();

    match level {
        1 => {
            // Mild: gutter 2mm -> 1.2mm
            result = result.replace("gutter: 2mm", "gutter: 1.2mm");
            // Body text: size: 5pt -> 4.8pt
            result = result.replace("size: 5pt,\n  lang:", "size: 4.8pt,\n  lang:");
            // Heading block: above: 0.8pt, below: 0.4pt -> 0.5pt, 0.2pt
            result = result.replace(
                "set block(above: 0.8pt, below: 0.4pt)",
                "set block(above: 0.5pt, below: 0.2pt)",
            );
        }
        2 => {
            // Aggressive: gutter 2mm -> 0.8mm
            result = result.replace("gutter: 2mm", "gutter: 0.8mm");
            // Body text: size: 5pt -> 4.5pt
            result = result.replace("size: 5pt,\n  lang:", "size: 4.5pt,\n  lang:");
            // Heading block: above: 0.8pt, below: 0.4pt -> 0.3pt, 0.1pt
            result = result.replace(
                "set block(above: 0.8pt, below: 0.4pt)",
                "set block(above: 0.3pt, below: 0.1pt)",
            );
        }
        _ => {} // unknown level - no change
    }

    result
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

/// Translate common LaTeX math commands inside `$...$` to Typst-compatible
/// forms.  Handles commands that Typst doesn't recognise natively in math mode.
fn translate_latex_math_to_typst(math: &str) -> String {
    // Common LaTeX → Unicode / Typst translations for math commands.
    // Sorted roughly by frequency of use in course notes.
    let replacements: &[(&str, &str)] = &[
        // Greek letters (LaTeX command → Unicode)
        (r"\alpha", "α"),
        (r"\beta", "β"),
        (r"\gamma", "γ"),
        (r"\delta", "δ"),
        (r"\epsilon", "ε"),
        (r"\varepsilon", "ε"),
        (r"\zeta", "ζ"),
        (r"\eta", "η"),
        (r"\theta", "θ"),
        (r"\vartheta", "ϑ"),
        (r"\iota", "ι"),
        (r"\kappa", "κ"),
        (r"\lambda", "λ"),
        (r"\mu", "μ"),
        (r"\nu", "ν"),
        (r"\xi", "ξ"),
        (r"\pi", "π"),
        (r"\varpi", "ϖ"),
        (r"\rho", "ρ"),
        (r"\varrho", "ϱ"),
        (r"\sigma", "σ"),
        (r"\varsigma", "ς"),
        (r"\tau", "τ"),
        (r"\upsilon", "υ"),
        (r"\phi", "φ"),
        (r"\varphi", "ϕ"),
        (r"\chi", "χ"),
        (r"\psi", "ψ"),
        (r"\omega", "ω"),
        (r"\Gamma", "Γ"),
        (r"\Delta", "Δ"),
        (r"\Theta", "Θ"),
        (r"\Lambda", "Λ"),
        (r"\Xi", "Ξ"),
        (r"\Pi", "Π"),
        (r"\Sigma", "Σ"),
        (r"\Upsilon", "Υ"),
        (r"\Phi", "Φ"),
        (r"\Psi", "Ψ"),
        (r"\Omega", "Ω"),
        // Binary operators / relations
        (r"\propto", "∝"),
        (r"\approx", "≈"),
        (r"\equiv", "≡"),
        (r"\neq", "≠"),
        (r"\leq", "≤"),
        (r"\geq", "≥"),
        (r"\ll", "≪"),
        (r"\gg", "≫"),
        (r"\sim", "∼"),
        (r"\simeq", "≃"),
        (r"\cong", "≅"),
        (r"\doteq", "≐"),
        (r"\subset", "⊂"),
        (r"\supset", "⊃"),
        (r"\subseteq", "⊆"),
        (r"\supseteq", "⊇"),
        (r"\in", "∈"),
        (r"\notin", "∉"),
        (r"\ni", "∋"),
        (r"\forall", "∀"),
        (r"\exists", "∃"),
        (r"\neg", "¬"),
        (r"\land", "∧"),
        (r"\lor", "∨"),
        (r"\to", "→"),
        (r"\rightarrow", "→"),
        (r"\Rightarrow", "⇒"),
        (r"\leftarrow", "←"),
        (r"\Leftarrow", "⇐"),
        (r"\leftrightarrow", "↔"),
        (r"\mapsto", "↦"),
        (r"\uparrow", "↑"),
        (r"\downarrow", "↓"),
        (r"\times", "×"),
        (r"\cdot", "⋅"),
        (r"\div", "÷"),
        (r"\pm", "±"),
        (r"\mp", "∓"),
        (r"\oplus", "⊕"),
        (r"\otimes", "⊗"),
        (r"\odot", "⊙"),
        (r"\infty", "∞"),
        (r"\partial", "∂"),
        (r"\nabla", "∇"),
        (r"\int", "∫"),
        (r"\sum", "∑"),
        (r"\prod", "∏"),
        (r"\cap", "∩"),
        (r"\cup", "∪"),
        (r"\emptyset", "∅"),
        // Common notations
        (r"\ldots", "…"),
        (r"\cdots", "⋯"),
        (r"\vdots", "⋮"),
        (r"\ddots", "⋱"),
        (r"\angle", "∠"),
        (r"\triangle", "△"),
        (r"\square", "□"),
        (r"\Box", "□"),
        (r"\Re", "ℜ"),
        (r"\Im", "ℑ"),
        (r"\aleph", "ℵ"),
        // Delimiters
        (r"\langle", "⟨"),
        (r"\rangle", "⟩"),
        (r"\lceil", "⌈"),
        (r"\rceil", "⌉"),
        (r"\lfloor", "⌊"),
        (r"\rfloor", "⌋"),
    ];

    let mut result = math.to_string();
    // Sort by descending length to avoid partial matches (e.g. \rightarrow before \to).
    let mut sorted: Vec<&(&str, &str)> = replacements.iter().collect();
    sorted.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    for (latex, typst) in &sorted {
        result = result.replace(latex, typst);
    }
    result
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
            // Translate common LaTeX math commands inside $...$ to Typst-compatible forms.
            let math_content = &after_start[1..end - 1]; // strip $...$
            let translated = translate_latex_math_to_typst(math_content);
            result.push('$');
            result.push_str(&translated);
            result.push('$');
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

    // Whether we are using the embedded default Typst template (safe to
    // regex-modify for layout tightening).  Custom user templates are
    // never touched.
    let using_default_typst_template = template_path.map_or(true, |p| {
        Path::new(p).extension().and_then(|e| e.to_str()) != Some("typ")
    });

    let mut last_error: Option<String> = None;
    // Track space utilisation computed via `typst query`; used both for
    // overflow decisions and underflow reporting.
    let mut space_utilization: Option<SpaceUtilization> = None;

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
        let original_chars = md_content.chars().count();
        let working_content = if attempt > 0 {
            let compressed = compress_content(&md_content, attempt);
            let compressed_chars = compressed.chars().count();
            log::info!(
                "compression: {} → {} chars ({:.0}% reduction, level {})",
                original_chars,
                compressed_chars,
                (1.0 - compressed_chars as f64 / original_chars.max(1) as f64) * 100.0,
                attempt
            );
            compressed
        } else {
            log::info!("input content: {} chars (uncompressed)", original_chars);
            md_content.clone()
        };

        // --- inner compilation helper (returns (pdf_path, template_name, page_count)) ---
        // Tries Typst first; falls back to LaTeX on failure.
        let compile_with_template =
            |typst_template_str: &str, label: &str| -> Result<(String, String, usize)> {
                let mut errors: Vec<String> = Vec::new();

                // Try Typst first.
                if !typst_compiler.is_empty() {
                    let typst_body = markdown_to_typst(&working_content);
                    let filled = typst_template_str.replace("{{content}}", &typst_body);
                    match fs::write(&typ_path, &filled) {
                        Ok(_) => match compile_typst(
                            &typ_path.to_string_lossy(),
                            &intermediate_pdf_path.to_string_lossy(),
                            &typst_compiler,
                        ) {
                            Ok(_) => {
                                let page_count =
                                    count_pdf_pages(&intermediate_pdf_path.to_string_lossy())?;
                                return Ok((
                                    intermediate_pdf_path.to_string_lossy().to_string(),
                                    label.to_string(),
                                    page_count,
                                ));
                            }
                            Err(e) => errors.push(format!("typst: {:#}", e)),
                        },
                        Err(e) => errors.push(format!("typst write: {:#}", e)),
                    }
                }

                // Fall back to LaTeX.
                if !compiler.is_empty() {
                    let latex_body = markdown_to_latex(&working_content);
                    let filled = template.replace("{{content}}", &latex_body);
                    match fs::write(&tex_path, &filled) {
                        Ok(_) => match compile_latex(
                            &tex_path.to_string_lossy(),
                            &output_dir.to_string_lossy(),
                            &compiler,
                        ) {
                            Ok(_) => {
                                let page_count =
                                    count_pdf_pages(&intermediate_pdf_path.to_string_lossy())?;
                                return Ok((
                                    intermediate_pdf_path.to_string_lossy().to_string(),
                                    label.to_string(),
                                    page_count,
                                ));
                            }
                            Err(e) => errors.push(format!("latex: {:#}", e)),
                        },
                        Err(e) => errors.push(format!("latex write: {:#}", e)),
                    }
                }

                if errors.is_empty() {
                    bail!("no PDF renderer available")
                } else {
                    bail!("{}", errors.join("; "))
                }
            };

        // --- compile ---
        let template_label = if !typst_compiler.is_empty() {
            &typst_template_name
        } else {
            &latex_template_name
        };
        let (pdf_path, used_template, page_count) =
            match compile_with_template(&typst_template, template_label) {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("compilation failed: {:#}", e);
                    log::warn!("{}", msg);
                    crate::utils::output::web_log(format!(
                        "render_cheatsheet: compilation FAILED attempt={}: {}",
                        attempt, msg,
                    ));
                    last_error = Some(msg);
                    continue;
                }
            };

        // Preserve intermediate files in debug mode.
        if std::env::var("LECTURE_DISTILL_DEBUG").unwrap_or_default() == "1" {
            let attempt_typ = output_dir.join(format!("cheatsheet_attempt{}.typ", attempt));
            let attempt_md = output_dir.join(format!("cheatsheet_attempt{}.md", attempt));
            let _ = fs::copy(&typ_path, &attempt_typ);
            let _ = fs::write(&attempt_md, &working_content);
            log::info!(
                "debug: preserved intermediate files for attempt {}: {}, {}",
                attempt,
                attempt_typ.display(),
                attempt_md.display()
            );
        }

        let input_chars_for_log = working_content.chars().count();
        let chars_per_page_log = input_chars_for_log as f64 / page_count.max(1) as f64;
        log::info!("compiled PDF: {} pages (attempt {})", page_count, attempt);
        crate::utils::output::web_log(format!(
            "render_cheatsheet: compiled attempt={} pages={}/{} input_chars={} chars_per_page={:.0}",
            attempt, page_count, max_pages, input_chars_for_log, chars_per_page_log,
        ));

        // ================================================================
        // Success path — within page limit
        // ================================================================
        if page_count <= max_pages {
            // Detect under-utilisation via typst query (best-effort).
            if !typst_compiler.is_empty() {
                space_utilization = try_compute_space_utilization(
                    &typ_path.to_string_lossy(),
                    &typst_compiler,
                    page_count,
                    max_pages,
                );
            }

            fs::copy(&pdf_path, &out_pdf).with_context(|| {
                format!(
                    "failed to copy PDF from {} to {}",
                    pdf_path,
                    out_pdf.display()
                )
            })?;

            // Compute chars/page ratio for diagnostics.
            let input_chars = working_content.chars().count();
            let chars_per_page = input_chars as f64 / page_count.max(1) as f64;
            let fill_pct = chars_per_page / 11000.0 * 100.0;
            let total_target = max_pages.saturating_mul(11000);
            let total_fill_pct = input_chars as f64 / total_target.max(1) as f64 * 100.0;
            let under_util = space_utilization
                .as_ref()
                .map_or(false, |su| su.last_page_under_utilized);
            log::info!(
                "cheat sheet rendered successfully: {} ({} pages, {} compression attempts, {} input chars, {:.0} chars/page, {:.0}% per-page fill, {:.0}% total fill{})",
                out_pdf.display(),
                page_count,
                attempt,
                input_chars,
                chars_per_page,
                fill_pct,
                total_fill_pct,
                if under_util {
                    ", last page under-utilised"
                } else {
                    ""
                }
            );
            crate::utils::output::web_log(format!(
                "render_cheatsheet: SUCCESS attempt={} pages={}/{} input_chars={} chars_per_page={:.0} per_page_fill={:.0}% total_fill={:.0}% under_util={} space_util={:?}",
                attempt,
                page_count,
                max_pages,
                input_chars,
                chars_per_page,
                fill_pct,
                total_fill_pct,
                under_util,
                space_utilization.as_ref().map(|su| format!(
                    "total_content={:.2} last_page={:.1}%",
                    su.total_content_pages,
                    su.last_page_utilization_pct,
                )),
            ));

            return Ok(CheatSheetArtifact {
                pdf_path: out_pdf.to_string_lossy().to_string(),
                page_count,
                template_used: used_template.to_string(),
                distilled_content_path: input_md_path.to_string(),
                rendered_at: chrono::Utc::now().to_rfc3339(),
                compression_attempts: attempt,
                space_utilization,
            });
        }

        // ================================================================
        // Overflow path — check if marginal via typst query
        // ================================================================

        if !typst_compiler.is_empty() && using_default_typst_template {
            if let Some(su) = try_compute_space_utilization(
                &typ_path.to_string_lossy(),
                &typst_compiler,
                page_count,
                max_pages,
            ) {
                if let Some(overflow_ratio) = su.overflow_ratio {
                    if overflow_ratio < LAYOUT_TIGHTEN_OVERFLOW_THRESHOLD {
                        log::info!(
                            "marginal overflow: ratio={:.2}% — attempting layout tightening",
                            overflow_ratio * 100.0
                        );

                        for layout_level in 1..=2 {
                            let tighter =
                                generate_tighter_typst_template(&typst_template, layout_level);
                            let layout_label =
                                format!("default_cheatsheet.typ (tight L{})", layout_level);

                            match compile_with_template(&tighter, &layout_label) {
                                Ok((_lp, lt, lc)) => {
                                    log::info!(
                                        "layout-tightened PDF: {} pages (level {})",
                                        lc,
                                        layout_level
                                    );
                                    if lc <= max_pages {
                                        // Success with layout-only fix!
                                        fs::copy(&intermediate_pdf_path, &out_pdf).with_context(
                                            || {
                                                format!(
                                                    "failed to copy PDF from {} to {}",
                                                    intermediate_pdf_path.display(),
                                                    out_pdf.display()
                                                )
                                            },
                                        )?;

                                        let input_chars = working_content.chars().count();
                                        let chars_per_page = input_chars as f64 / lc.max(1) as f64;
                                        log::info!(
                                            "cheat sheet rendered with layout tightening: {} ({} pages, level {}, {} input chars, {:.0} chars/page)",
                                            out_pdf.display(),
                                            lc,
                                            layout_level,
                                            input_chars,
                                            chars_per_page
                                        );

                                        // Re-check utilisation after tightening.
                                        let su_after = if !typst_compiler.is_empty() {
                                            try_compute_space_utilization(
                                                &typ_path.to_string_lossy(),
                                                &typst_compiler,
                                                lc,
                                                max_pages,
                                            )
                                        } else {
                                            None
                                        };

                                        return Ok(CheatSheetArtifact {
                                            pdf_path: out_pdf.to_string_lossy().to_string(),
                                            page_count: lc,
                                            template_used: lt.to_string(),
                                            distilled_content_path: input_md_path.to_string(),
                                            rendered_at: chrono::Utc::now().to_rfc3339(),
                                            compression_attempts: attempt,
                                            space_utilization: su_after,
                                        });
                                    }
                                }
                                Err(e) => {
                                    log::warn!(
                                        "layout tightening level {} failed: {:#}",
                                        layout_level,
                                        e
                                    );
                                }
                            }
                        }

                        log::info!(
                            "layout tightening did not bring pages within limit — falling back to content compression"
                        );
                    } else {
                        log::info!(
                            "overflow ratio {:.2}% exceeds layout-tightening threshold {:.0}% — going straight to content compression",
                            overflow_ratio * 100.0,
                            LAYOUT_TIGHTEN_OVERFLOW_THRESHOLD * 100.0
                        );
                    }
                }
            }
        }

        // Too many pages — log and retry with more compression.
        log::warn!(
            "PDF has {} pages, exceeding max {} (attempt {})",
            page_count,
            max_pages,
            attempt
        );
        crate::utils::output::web_log(format!(
            "render_cheatsheet: OVERFLOW attempt={} pages={}/{} input_chars={} — retrying with compression",
            attempt, page_count, max_pages, input_chars_for_log,
        ));
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

    // -- parse_typst_length_to_mm ----------------------------------------

    #[test]
    fn parse_typst_length_mm() {
        assert!((parse_typst_length_to_mm("10.2mm").unwrap() - 10.2).abs() < 0.001);
        assert!((parse_typst_length_to_mm("0mm").unwrap() - 0.0).abs() < 0.001);
        assert!((parse_typst_length_to_mm("  5mm ").unwrap() - 5.0).abs() < 0.001);
    }

    #[test]
    fn parse_typst_length_pt() {
        let val = parse_typst_length_to_mm("5pt").unwrap();
        assert!((val - 1.764).abs() < 0.01); // 5 * 0.3528
    }

    #[test]
    fn parse_typst_length_cm() {
        let val = parse_typst_length_to_mm("1.5cm").unwrap();
        assert!((val - 15.0).abs() < 0.001);
    }

    #[test]
    fn parse_typst_length_in() {
        let val = parse_typst_length_to_mm("1in").unwrap();
        assert!((val - 25.4).abs() < 0.001);
    }

    #[test]
    fn parse_typst_length_no_unit_assumes_mm() {
        let val = parse_typst_length_to_mm("42.0").unwrap();
        assert!((val - 42.0).abs() < 0.001);
    }

    #[test]
    fn parse_typst_length_empty_returns_none() {
        assert!(parse_typst_length_to_mm("").is_none());
        assert!(parse_typst_length_to_mm("   ").is_none());
    }

    // -- compute_space_utilization ---------------------------------------

    #[test]
    fn compute_utilization_single_page_full() {
        let elements = vec![
            TypstElement {
                page: 1,
                y_mm: 50.0,
                element_type: "heading".into(),
            },
            TypstElement {
                page: 1,
                y_mm: 150.0,
                element_type: "par".into(),
            },
            TypstElement {
                page: 1,
                y_mm: 280.0,
                element_type: "par".into(),
            },
        ];
        let su = compute_space_utilization(&elements, 1, 1);
        assert!(su.overflow_ratio.is_none());
        assert!(!su.last_page_under_utilized); // 280/297 ≈ 94% > 75%
        assert!((su.last_page_utilization_pct - 94.27).abs() < 0.5);
    }

    #[test]
    fn compute_utilization_last_page_under_utilized() {
        let elements = vec![
            TypstElement {
                page: 1,
                y_mm: 280.0,
                element_type: "par".into(),
            },
            TypstElement {
                page: 2,
                y_mm: 30.0,
                element_type: "heading".into(),
            },
        ];
        let su = compute_space_utilization(&elements, 2, 2);
        assert!(su.overflow_ratio.is_none());
        assert!(su.last_page_under_utilized); // 30/297 ≈ 10% < 75%
        assert!(su.last_page_utilization_pct < 20.0);
    }

    #[test]
    fn compute_utilization_overflow_ratio() {
        // 2 full pages + 1 page at 20% utilization → 2.20 effective pages
        let elements = vec![
            TypstElement {
                page: 1,
                y_mm: 290.0,
                element_type: "par".into(),
            },
            TypstElement {
                page: 2,
                y_mm: 285.0,
                element_type: "par".into(),
            },
            TypstElement {
                page: 3,
                y_mm: 59.4,
                element_type: "par".into(),
            },
        ];
        let su = compute_space_utilization(&elements, 3, 2);
        assert!(su.overflow_ratio.is_some());
        let ratio = su.overflow_ratio.unwrap();
        // page 1: ~97.6%, page 2: ~96.0%, page 3: 20% → total ≈ 2.136
        // ratio = 2.136/2 ≈ 1.068
        assert!(ratio > 1.0);
        assert!(ratio < 1.25); // marginal overflow → should trigger layout tightening
        assert!((su.total_content_pages - 2.13).abs() < 0.2);
    }

    #[test]
    fn compute_utilization_severe_overflow() {
        // 3 full pages + tail → way beyond layout tightening threshold
        let elements = vec![
            TypstElement {
                page: 1,
                y_mm: 290.0,
                element_type: "par".into(),
            },
            TypstElement {
                page: 2,
                y_mm: 290.0,
                element_type: "par".into(),
            },
            TypstElement {
                page: 3,
                y_mm: 290.0,
                element_type: "par".into(),
            },
        ];
        let su = compute_space_utilization(&elements, 3, 2);
        let ratio = su.overflow_ratio.unwrap();
        assert!(ratio > 1.25); // severe overflow — should skip layout tightening
    }

    // -- generate_tighter_typst_template ---------------------------------

    #[test]
    fn tighter_template_level_1_changes_gutter() {
        let result = generate_tighter_typst_template(DEFAULT_TYPST_TEMPLATE, 1);
        assert!(!result.contains("gutter: 2mm"));
        assert!(result.contains("gutter: 1.2mm"));
        // Body font should shrink.
        assert!(result.contains("size: 4.8pt"));
        assert!(!result.contains("size: 5pt,\n  lang:"));
        // Heading spacing should tighten.
        assert!(!result.contains("set block(above: 0.8pt, below: 0.4pt)"));
        assert!(result.contains("set block(above: 0.5pt, below: 0.2pt)"));
    }

    #[test]
    fn tighter_template_level_2_changes_gutter_more() {
        let result = generate_tighter_typst_template(DEFAULT_TYPST_TEMPLATE, 2);
        assert!(result.contains("gutter: 0.8mm"));
        assert!(result.contains("size: 4.5pt"));
        assert!(result.contains("set block(above: 0.3pt, below: 0.1pt)"));
    }

    #[test]
    fn tighter_template_level_0_is_noop() {
        let result = generate_tighter_typst_template(DEFAULT_TYPST_TEMPLATE, 0);
        assert_eq!(result, DEFAULT_TYPST_TEMPLATE);
    }
}
