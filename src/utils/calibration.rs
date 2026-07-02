//! Template calibration: measure how many content characters fit on one page
//! for a given Typst template, separately for CJK and Latin content.
//!
//! Calibration works by generating structured sample Markdown, compiling it
//! with the target template via `typst compile`, then running `typst query`
//! on the end-marker to determine the exact page position.  Results are
//! cached as `<template>.calibration.json`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Per-template calibration data, stored as JSON next to the template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationData {
    /// Template filename (for identification only).
    pub template: String,
    /// ISO-8601 timestamp of calibration.
    pub calibrated_at: String,
    /// Effective CJK characters that fit on one page with this template.
    pub cjk_chars_per_page: usize,
    /// Effective English words that fit on one page (for LLM-facing targets).
    pub english_words_per_page: usize,
    /// Effective Latin characters that fit on one page (for internal budget blending).
    pub english_chars_per_page: usize,
    /// Usable page height in mm (from typst query end-marker).
    pub page_height_mm: f64,
    /// Usable page width in mm (for diagnostics).
    pub page_width_mm: f64,
}

/// Deprecated fallback constants used when typst is unavailable.
/// These are the old 4-col 5pt values; they will drift further from
/// reality as templates evolve.
pub const DEPRECATED_CJK_PER_PAGE: usize = 8500;
/// Soft-max ratio: `target_per_page * 1.4` → soft maximum chars/page.
pub const SOFT_MAX_RATIO: f64 = 1.4;

/// A structured Markdown sample that exercises all major cheat sheet
/// structural elements: h1, h2, h3 headings, unordered lists, ordered
/// lists, inline code, bold, and paragraph text.
///
/// The CJK version uses Chinese text; the English version uses Latin text.
/// Both have identical structure so the calibration result reflects
/// real-world cheat sheet composition.
const CALIBRATION_SAMPLE_CJK: &str = r##"# 章节标题
这是包含行内数学公式 $x^2 + y^2 = z^2$ 的段落文本内容。

## 小节标题
- 第一项要点描述
- 第二项包含**粗体**关键词的要点
- 第三项

### 细目
1. 第一步操作步骤描述
2. 第二步带有 `代码` 标识符的描述

这是跨越多句的段落文字用于填满横向空间以便测量列布局的实际容纳量。

## 另一个小节
- 更多列表内容
- 公式展示：$\sum_{i=1}^{n} i = \frac{n(n+1)}{2}$
- 考试判断准则：当 $x \to 0$ 时，$\sin x \approx x$

### 注意事项
1. 常见错误一：混淆定义与性质
2. 常见错误二：忽略前提条件

## 关键概念
段落文字描述了核心概念的定义和适用条件包含**重要公式** $E = mc^2$。
- 条件 A：满足某某假设时成立
- 条件 B：仅在特定范围内有效"##;

const CALIBRATION_SAMPLE_ENGLISH: &str = r##"# Section Title
This is paragraph text containing inline math expressions $x^2 + y^2 = z^2$.

## Subsection Heading
- First bullet point description
- Second bullet with **bold** key term
- Third item

### Sub-subsection
1. First numbered step description
2. Second step with `code` identifier

Paragraph text that spans multiple sentences to fill horizontal space for measuring column layout capacity.

## Another Subsection
- More bullet content here
- Formula display: $\sum_{i=1}^{n} i = \frac{n(n+1)}{2}$
- Exam judgement rule: when $x \to 0$, $\sin x \approx x$

### Important Notes
1. Common mistake one: confusing definition with property
2. Common mistake two: ignoring preconditions

## Key Concepts
Paragraph text describing core concept definitions and applicability conditions with **important formula** $E = mc^2$.
- Condition A: holds under certain assumptions
- Condition B: valid only within specific range"##;

/// Generate the CJK (Chinese) calibration sample.
/// Returns Markdown whose effective CJK char count is known.
pub fn generate_cjk_sample() -> String {
    CALIBRATION_SAMPLE_CJK.to_string()
}

/// Generate the English calibration sample.
/// Returns Markdown whose effective Latin char count is known.
pub fn generate_english_sample() -> String {
    CALIBRATION_SAMPLE_ENGLISH.to_string()
}

/// Known effective CJK char count for the bundled CJK sample.
/// Computed by running `count_effective` on the sample and recording the result.
/// This is a pre-computed constant to avoid a circular dependency on budget.rs.
pub fn sample_effective_cjk_chars() -> usize {
    239
}

/// Known effective Latin char count for the bundled English sample.
pub fn sample_effective_english_chars() -> usize {
    684
}

// ---------------------------------------------------------------------------
// Typst compiler discovery
// ---------------------------------------------------------------------------

/// Find the typst compiler, same logic as `find_latex_compiler()` in latex.rs.
///
/// On Windows, searches for `typst.exe`; on other platforms, `typst`.
/// Returns the binary name (with `.exe` suffix on Windows — the caller
/// passes it directly to `Command::new`).
fn find_typst() -> Result<String> {
    let candidates: &[&str] = if cfg!(windows) {
        &["typst.exe"]
    } else {
        &["typst"]
    };
    for name in candidates {
        match Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => return Ok(name.to_string()),
            _ => {}
        }
    }
    bail!("typst not found on PATH — install Typst to enable template calibration")
}

// ---------------------------------------------------------------------------
// Per-page capacity measurement via typst compile + query
// ---------------------------------------------------------------------------

/// Compile sample Markdown with the given template and measure how many
/// effective chars fit on one page via `typst query` on the end-marker.
///
/// Returns `effective_chars / total_content_pages` rounded to the nearest
/// integer.
fn measure_chars_per_page(
    md: &str,
    effective_chars: usize,
    template_str: &str,
    typst_exe: &str,
    work_dir: &Path,
) -> Result<usize> {
    let (chars_per_page, _height, _width) =
        measure_per_page_capacity(md, effective_chars, template_str, typst_exe, work_dir)?;
    Ok(chars_per_page)
}

/// Compile sample Markdown with the given template and measure layout
/// capacity via `typst query` on the end-marker.
///
/// Returns `(chars_per_page, page_height_mm, page_width_mm)` where
/// `chars_per_page` = `effective_chars / total_content_pages`.
fn measure_per_page_capacity(
    md: &str,
    effective_chars: usize,
    template_str: &str,
    typst_exe: &str,
    work_dir: &Path,
) -> Result<(usize, f64, f64)> {
    let typst_body = crate::latex::markdown_to_typst(md);
    let filled = template_str.replace("{{content}}", &typst_body);

    let typ_path = work_dir.join("_calibration.typ");
    let pdf_path = work_dir.join("_calibration.pdf");

    std::fs::write(&typ_path, &filled).with_context(|| "failed to write calibration .typ file")?;

    // Compile
    let output = Command::new(typst_exe)
        .args([
            "compile",
            typ_path.to_str().unwrap(),
            pdf_path.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to run typst compile for calibration")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Clean up before bailing
        let _ = std::fs::remove_file(&typ_path);
        let _ = std::fs::remove_file(&pdf_path);
        bail!("typst compile failed during calibration: {}", stderr.trim());
    }

    // Query end-marker
    let query_output = Command::new(typst_exe)
        .args([
            "query",
            typ_path.to_str().unwrap(),
            "metadata",
            "--field",
            "value",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to run typst query for calibration")?;

    let stdout = String::from_utf8_lossy(&query_output.stdout);
    let values: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .context("failed to parse typst query output during calibration")?;

    let last_marker = values
        .iter()
        .rev()
        .find(|v| v.get("kind").and_then(|k| k.as_str()) == Some("lecture-distill-end-marker"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no end-marker found in calibration output ({} metadata entries)",
                values.len()
            )
        })?;

    let end_page: usize = last_marker["end_page"]
        .as_u64()
        .context("end_marker missing end_page")? as usize;
    let end_y_pt: f64 = last_marker["end_y_pt"]
        .as_f64()
        .context("end_marker missing end_y_pt")?;
    let end_y_mm = end_y_pt * 0.3528;

    // Conservative usable page height: A4 = 297mm, subtract typical margins.
    let usable_height_mm: f64 = 290.0;
    let usable_width_mm: f64 = 200.0; // A4 210mm minus ~10mm margins

    let total_content_pages = (end_page.saturating_sub(1)) as f64 + end_y_mm / usable_height_mm;
    let total_pages = total_content_pages.max(0.5); // at least half a page
    let chars_per_page = (effective_chars as f64 / total_pages).round() as usize;

    // Clean up temp files
    let _ = std::fs::remove_file(&typ_path);
    let _ = std::fs::remove_file(&pdf_path);

    Ok((chars_per_page, usable_height_mm, usable_width_mm))
}

// ---------------------------------------------------------------------------
// Full template calibration
// ---------------------------------------------------------------------------

/// Run calibration for a template file and return the results.
///
/// Generates both CJK and English structured samples, compiles each with
/// the template, and measures per-page capacity via `typst query`.
///
/// The `template_path` must point to an existing `.typ` file.
/// `project_dir` is used for temporary compilation artifacts.
pub fn calibrate_template(template_path: &Path, project_dir: &Path) -> Result<CalibrationData> {
    let typst_exe = find_typst()?;

    let template_str = std::fs::read_to_string(template_path)
        .with_context(|| format!("failed to read template: {}", template_path.display()))?;

    if !template_str.contains("{{content}}") {
        bail!("template does not contain required {{content}} placeholder");
    }

    let work_dir = project_dir.join("calibrations").join("_work");
    std::fs::create_dir_all(&work_dir)
        .with_context(|| "failed to create calibration work directory")?;

    let template_name = template_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown.typ".to_string());

    // Measure CJK capacity
    let cjk_sample = generate_cjk_sample();
    let cjk_effective = sample_effective_cjk_chars();
    let (cjk_per_page, height_mm, width_mm) = measure_per_page_capacity(
        &cjk_sample,
        cjk_effective,
        &template_str,
        &typst_exe,
        &work_dir,
    )?;

    // Measure English capacity
    let eng_sample = generate_english_sample();
    let eng_effective = sample_effective_english_chars();
    let (eng_chars_per_page, _h, _w) = measure_per_page_capacity(
        &eng_sample,
        eng_effective,
        &template_str,
        &typst_exe,
        &work_dir,
    )?;

    // English words: divide chars by 5 (standard avg word length)
    let eng_words_per_page = eng_chars_per_page / 5;

    // Clean up work directory
    let _ = std::fs::remove_dir_all(&work_dir);

    Ok(CalibrationData {
        template: template_name,
        calibrated_at: chrono::Utc::now().to_rfc3339(),
        cjk_chars_per_page: cjk_per_page,
        english_words_per_page: eng_words_per_page,
        english_chars_per_page: eng_chars_per_page,
        page_height_mm: height_mm,
        page_width_mm: width_mm,
    })
}

// ---------------------------------------------------------------------------
// Calibration cache helpers
// ---------------------------------------------------------------------------

/// Compute the cache key for a template.
///
/// For a file template: uses the filename stem (e.g. "default_cheatsheet").
/// For the embedded default: returns `"__default__"`.
fn calibration_cache_key(template_path: Option<&Path>) -> String {
    match template_path {
        Some(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "__default__".to_string()),
        None => "__default__".to_string(),
    }
}

/// Determine the calibration JSON path for a given template.
///
/// For a file template: places `<stem>.calibration.json` next to the
/// template file.  For the embedded default: places
/// `<project_dir>/calibrations/__default__.calibration.json`.
fn calibration_path(template_path: Option<&Path>, project_dir: &Path) -> PathBuf {
    match template_path {
        Some(p) => {
            // Place .calibration.json next to the template file.
            let mut cal = p.to_path_buf();
            cal.set_extension("calibration.json");
            cal
        }
        None => {
            // Embedded default: cache in project dir.
            project_dir
                .join("calibrations")
                .join("__default__.calibration.json")
        }
    }
}

// ---------------------------------------------------------------------------
// Loading and lazy calibration (public entry points)
// ---------------------------------------------------------------------------

/// Load cached calibration data from disk.
///
/// Returns `Ok(CalibrationData)` if a valid calibration file exists,
/// or `Err(...)` if it is missing / corrupt / from an older version.
pub fn load_calibration(
    template_path: Option<&Path>,
    project_dir: &Path,
) -> Result<CalibrationData> {
    let path = calibration_path(template_path, project_dir);
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("no calibration file at {}", path.display()))?;
    let calib: CalibrationData =
        serde_json::from_str(&data).context("failed to parse calibration JSON")?;
    // Basic validation
    if calib.cjk_chars_per_page == 0 || calib.english_chars_per_page == 0 {
        bail!("calibration data has zero capacity values");
    }
    Ok(calib)
}

/// Ensure calibration data exists, running calibration if needed.
///
/// This is the main entry point for lazy calibration (plan B).
/// If a valid calibration file exists it is returned immediately;
/// otherwise the template is compiled and measured automatically.
///
/// Falls back to deprecated hardcoded constants when typst is
/// unavailable (e.g. in a headless CI environment).  This function
/// **never panics** — it always returns a `CalibrationData`.
pub fn ensure_calibration(template_path: Option<&Path>, project_dir: &Path) -> CalibrationData {
    match load_calibration(template_path, project_dir) {
        Ok(calib) => {
            log::info!(
                "calibration loaded: cjk={}/page eng={}/page",
                calib.cjk_chars_per_page,
                calib.english_chars_per_page,
            );
            calib
        }
        Err(_) => {
            // Try to calibrate if we have a concrete template path.
            if let Some(tp) = template_path {
                if tp.exists() {
                    log::info!("running calibration for {}", tp.display());
                    match calibrate_template(tp, project_dir) {
                        Ok(calib) => {
                            // Save calibration for next time.
                            let cal_path = calibration_path(Some(tp), project_dir);
                            if let Some(parent) = cal_path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if let Ok(json) = serde_json::to_string_pretty(&calib) {
                                if let Err(e) = std::fs::write(&cal_path, &json) {
                                    log::warn!(
                                        "failed to save calibration to {}: {}",
                                        cal_path.display(),
                                        e,
                                    );
                                }
                            }
                            log::info!(
                                "calibration complete: cjk={}/page eng={}/page (saved to {})",
                                calib.cjk_chars_per_page,
                                calib.english_chars_per_page,
                                cal_path.display(),
                            );
                            return calib;
                        }
                        Err(e) => {
                            log::warn!("calibration failed: {:#} — using deprecated defaults", e,);
                        }
                    }
                }
            }
            // Fallback
            log::warn!("using deprecated hardcoded budget constants");
            CalibrationData {
                template: "__fallback__".to_string(),
                calibrated_at: "never".to_string(),
                cjk_chars_per_page: DEPRECATED_CJK_PER_PAGE,
                english_words_per_page: DEPRECATED_CJK_PER_PAGE / 5,
                english_chars_per_page: DEPRECATED_CJK_PER_PAGE,
                page_height_mm: 290.0,
                page_width_mm: 200.0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::budget::count_effective;

    /// Verify that the hardcoded sample effective char counts match what
    /// `count_effective` computes.  If this fails, update the constants in
    /// `sample_effective_cjk_chars()` and `sample_effective_english_chars()`.
    #[test]
    fn sample_effective_counts_match_count_effective() {
        let cjk = count_effective(CALIBRATION_SAMPLE_CJK);
        let eng = count_effective(CALIBRATION_SAMPLE_ENGLISH);
        assert_eq!(
            cjk.total_significant,
            sample_effective_cjk_chars(),
            "CJK sample: count_effective returned {}, but sample_effective_cjk_chars() returns {}",
            cjk.total_significant,
            sample_effective_cjk_chars(),
        );
        assert_eq!(
            eng.total_significant,
            sample_effective_english_chars(),
            "ENG sample: count_effective returned {}, but sample_effective_english_chars() returns {}",
            eng.total_significant,
            sample_effective_english_chars(),
        );
    }
}
