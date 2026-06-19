//! Shared pipeline runner used by CLI and Web GUI.
//!
//! Provides stage methods that return structured `PipelineResult` objects
//! with status, artifact paths, errors, and logs.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::artifacts::SpaceUtilization;
use crate::canvas_sjtu::CanvasSJTUVideoClient;
use crate::distill;
use crate::latex;
use crate::notes;
use crate::transcripts::transcript_to_srt;

// ---------------------------------------------------------------------------
// Structured result types
// ---------------------------------------------------------------------------

/// Result of a single pipeline stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineResult {
    pub status: String, // "succeeded" | "failed" | "skipped"
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub logs: Vec<String>,
    /// Space utilisation info from `typst query` (populated for render stage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_utilization: Option<SpaceUtilization>,
}

impl PipelineResult {
    pub fn ok(&self) -> bool {
        self.status == "succeeded"
    }

    pub fn succeeded(artifact_paths: Vec<String>, logs: Vec<String>) -> Self {
        Self {
            status: "succeeded".to_string(),
            artifact_paths,
            errors: Vec::new(),
            logs,
            space_utilization: None,
        }
    }

    pub fn failed(errors: Vec<String>, logs: Vec<String>) -> Self {
        Self {
            status: "failed".to_string(),
            artifact_paths: Vec::new(),
            errors,
            logs,
            space_utilization: None,
        }
    }

    pub fn skipped() -> Self {
        Self {
            status: "skipped".to_string(),
            artifact_paths: Vec::new(),
            errors: Vec::new(),
            logs: Vec::new(),
            space_utilization: None,
        }
    }
}

/// Result of running the full pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRunResult {
    pub fetch: PipelineResult,
    pub patch: PipelineResult,
    pub distill: PipelineResult,
    pub render: PipelineResult,
}

impl PipelineRunResult {
    /// Creates a new result with all stages set to skipped.
    pub fn new() -> Self {
        Self {
            fetch: PipelineResult::skipped(),
            patch: PipelineResult::skipped(),
            distill: PipelineResult::skipped(),
            render: PipelineResult::skipped(),
        }
    }

    /// Returns true only when every stage status is `"succeeded"`.
    pub fn all_ok(&self) -> bool {
        self.fetch.ok() && self.patch.ok() && self.distill.ok() && self.render.ok()
    }
}

// ---------------------------------------------------------------------------
// PipelineRunner
// ---------------------------------------------------------------------------

/// Shared pipeline runner for CLI and Web GUI.
///
/// Wraps the same core functions as the CLI but returns structured
/// `PipelineResult` objects.
pub struct PipelineRunner {
    pub project_dir: String,
}

impl PipelineRunner {
    /// Create a new pipeline runner. Creates the project directory if it
    /// doesn't exist.  Accepts both `String` and `&str`.
    pub fn new(project_dir: impl Into<String>) -> Result<Self> {
        let dir = project_dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { project_dir: dir })
    }

    /// Resolve a relative path against the project directory.
    fn resolve(&self, rel_path: &str) -> String {
        Path::new(&self.project_dir)
            .join(rel_path)
            .to_string_lossy()
            .to_string()
    }

    // -----------------------------------------------------------------------
    // Stage: fetch_transcripts
    // -----------------------------------------------------------------------

    /// Fetch transcripts from SJTU Canvas.
    ///
    /// - Creates output dir under `project_dir`
    /// - Authenticates and fetches
    /// - If `video_id` is `Some`, fetches single video; else lists all and
    ///   fetches each
    /// - Per video: saves JSON artifact (`.json`) and SRT file (`.srt`)
    /// - Catches per-video errors and continues fetching others
    pub async fn fetch_transcripts(
        &self,
        course_id: &str,
        cookie: &str,
        video_id: Option<&str>,
        transcripts_dir: &str,
    ) -> PipelineResult {
        let mut logs: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut artifact_paths: Vec<String> = Vec::new();

        let out_dir = self.resolve(transcripts_dir);
        if let Err(e) = fs::create_dir_all(&out_dir) {
            return PipelineResult::failed(
                vec![format!("Failed to create output dir: {}", e)],
                logs,
            );
        }

        logs.push(format!("Connecting to Canvas course {}...", course_id));

        // Authenticate with the SJTU video platform.
        let mut client = CanvasSJTUVideoClient::new(course_id.to_string(), cookie.to_string());
        if let Err(e) = client.authenticate().await {
            return PipelineResult::failed(vec![format!("Authentication failed: {}", e)], logs);
        }
        logs.push("Authenticated with SJTU video platform.".to_string());

        // Fetch subtitles for the requested video(s).
        let artifacts = match video_id {
            Some(vid) => {
                logs.push(format!("Fetching single video: {}", vid));
                match client.fetch_subtitles(vid).await {
                    Ok(art) => vec![art],
                    Err(e) => {
                        return PipelineResult::failed(
                            vec![format!("Failed to fetch subtitles for {}: {}", vid, e)],
                            logs,
                        );
                    }
                }
            }
            None => {
                let videos = match client.list_videos().await {
                    Ok(v) => v,
                    Err(e) => {
                        return PipelineResult::failed(
                            vec![format!("Failed to list videos: {}", e)],
                            logs,
                        );
                    }
                };
                logs.push(format!("Found {} video(s).", videos.len()));

                let mut collected = Vec::new();
                for v in &videos {
                    match client.fetch_subtitles(&v.video_id).await {
                        Ok(art) => collected.push(art),
                        Err(e) => {
                            errors.push(format!("Failed {}: {}", v.video_id, e));
                        }
                    }
                }
                collected
            }
        };

        // Save transcript artifacts to disk.
        for art in &artifacts {
            let json_path = Path::new(&out_dir).join(format!("{}.json", art.video_id));
            match serde_json::to_string_pretty(art) {
                Ok(json) => {
                    if let Err(e) = fs::write(&json_path, &json) {
                        errors.push(format!("Failed to write {}: {}", json_path.display(), e));
                    } else {
                        artifact_paths.push(json_path.to_string_lossy().to_string());
                    }
                }
                Err(e) => {
                    errors.push(format!("Failed to serialize {}: {}", art.video_id, e));
                }
            }

            let srt_path = Path::new(&out_dir).join(format!("{}.srt", art.video_id));
            let srt_text = transcript_to_srt(art);
            if let Err(e) = fs::write(&srt_path, &srt_text) {
                errors.push(format!("Failed to write {}: {}", srt_path.display(), e));
            } else {
                artifact_paths.push(srt_path.to_string_lossy().to_string());
            }

            logs.push(format!(
                "Fetched {}: {} ({} segments)",
                art.video_id,
                art.video_title,
                art.segments.len()
            ));
        }

        logs.push(format!(
            "Saved {} transcript(s) to {}/",
            artifacts.len(),
            out_dir
        ));

        if errors.is_empty() {
            PipelineResult::succeeded(artifact_paths, logs)
        } else {
            PipelineResult::failed(errors, logs)
        }
    }

    // -----------------------------------------------------------------------
    // Stage: patch_notes
    // -----------------------------------------------------------------------

    /// Patch notes with transcript data.
    pub async fn patch_notes(
        &self,
        notes_path: &str,
        transcripts_dir: &str,
        output_notes: &str,
        output_patches: &str,
    ) -> PipelineResult {
        let mut logs: Vec<String> = Vec::new();

        let out_notes = self.resolve(output_notes);
        let out_patches = self.resolve(output_patches);
        let tx_dir = self.resolve(transcripts_dir);

        let notes_path = if Path::new(notes_path).is_absolute() {
            notes_path.to_string()
        } else {
            self.resolve(notes_path)
        };

        logs.push(format!("Patching notes: {}", notes_path));
        logs.push(format!("Transcripts dir: {}", tx_dir));

        match notes::patch_notes(&notes_path, &tx_dir, &out_notes, &out_patches).await {
            Ok(artifact) => {
                logs.push(format!("Applied {} patch(es).", artifact.patches.len()));
                logs.push(format!("Conflicts: {}", artifact.conflicts.len()));
                logs.push(format!("Patched notes -> {}", out_notes));
                logs.push(format!("Patches JSON -> {}", out_patches));

                PipelineResult::succeeded(vec![out_notes, out_patches], logs)
            }
            Err(e) => {
                logs.push(format!("Patch failed: {}", e));
                PipelineResult::failed(vec![e.to_string()], logs)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Stage: distill_notes
    // -----------------------------------------------------------------------

    /// Distill patched notes into exam-focused summary.
    pub async fn distill_notes(&self, notes_path: &str, output_path: &str) -> PipelineResult {
        let mut logs: Vec<String> = Vec::new();

        let out = self.resolve(output_path);
        let notes_path = if Path::new(notes_path).is_absolute() {
            notes_path.to_string()
        } else {
            self.resolve(notes_path)
        };

        logs.push(format!("Distilling: {}", notes_path));

        match distill::distill(&notes_path, &out).await {
            Ok(distilled) => {
                let lines = distilled.lines().count();
                let chars = distilled.len();
                logs.push(format!("Distilled -> {}", out));
                logs.push(format!("Output: {} lines, {} chars", lines, chars));

                PipelineResult::succeeded(vec![out], logs)
            }
            Err(e) => {
                logs.push(format!("Distillation failed: {}", e));
                PipelineResult::failed(vec![e.to_string()], logs)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Stage: render_pdf
    // -----------------------------------------------------------------------

    /// Render distilled notes to PDF cheat sheet.
    pub fn render_pdf(
        &self,
        input_md: &str,
        output_pdf: &str,
        template_path: Option<&str>,
        max_pages: usize,
    ) -> PipelineResult {
        let mut logs: Vec<String> = Vec::new();

        let out = self.resolve(output_pdf);
        let input_md = if Path::new(input_md).is_absolute() {
            input_md.to_string()
        } else {
            self.resolve(input_md)
        };

        logs.push(format!("Rendering: {}", input_md));
        logs.push(format!("Max pages: {}", max_pages));

        // Use catch_unwind to guard against panics in LaTeX rendering.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            latex::render_cheatsheet(&input_md, template_path, &out, max_pages)
        }));

        match result {
            Ok(Ok(artifact)) => {
                logs.push(format!("PDF -> {}", artifact.pdf_path));
                logs.push(format!("Pages: {}", artifact.page_count));
                logs.push(format!("Template: {}", artifact.template_used));
                logs.push(format!(
                    "Compression attempts: {}",
                    artifact.compression_attempts
                ));
                if let Some(ref su) = artifact.space_utilization {
                    logs.push(format!(
                        "Space utilisation: total={:.2} pages, last_page={:.1}%{}",
                        su.total_content_pages,
                        su.last_page_utilization_pct,
                        if su.last_page_under_utilized {
                            " (under-utilised)"
                        } else {
                            ""
                        }
                    ));
                }

                PipelineResult {
                    status: "succeeded".to_string(),
                    artifact_paths: vec![artifact.pdf_path],
                    errors: Vec::new(),
                    logs,
                    space_utilization: artifact.space_utilization,
                }
            }
            Ok(Err(e)) => {
                logs.push(format!("Render failed: {}", e));
                PipelineResult::failed(vec![e.to_string()], logs)
            }
            Err(panic) => {
                let msg = if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unknown panic".to_string()
                };
                logs.push(format!("Render panicked: {}", msg));
                PipelineResult::failed(vec![format!("Panic: {}", msg)], logs)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Full pipeline
    // -----------------------------------------------------------------------

    /// Run all stages sequentially: fetch -> patch -> distill -> render.
    ///
    /// Short-circuits if fetch fails (and no specific video_id was given) or
    /// patch fails.
    pub async fn run_all(
        &self,
        course_id: &str,
        cookie: &str,
        notes_path: &str,
        template_path: Option<&str>,
        output_pdf: &str,
        max_pages: usize,
        transcripts_dir: &str,
    ) -> PipelineRunResult {
        let mut result = PipelineRunResult::new();

        // Stage 1: Fetch transcripts.
        result.fetch = self
            .fetch_transcripts(course_id, cookie, None, transcripts_dir)
            .await;
        if !result.fetch.ok() {
            return result;
        }

        // Stage 2: Patch notes.
        result.patch = self
            .patch_notes(
                notes_path,
                transcripts_dir,
                "notes.patched.md",
                "patches.json",
            )
            .await;
        if !result.patch.ok() {
            return result;
        }

        // Stage 3: Distill.
        result.distill = self.distill_notes("notes.patched.md", "distilled.md").await;
        if !result.distill.ok() {
            return result;
        }

        // Stage 4: Render PDF.
        result.render = self.render_pdf("distilled.md", output_pdf, template_path, max_pages);

        // Stage 5 (optional): If the last page is under-utilised, attempt
        // LLM expansion and re-render (best-effort, max 1 attempt).
        if result.render.ok() {
            if let Some(ref su) = result.render.space_utilization {
                if su.last_page_under_utilized {
                    let target_pct = (1.0 - su.last_page_utilization_pct / 100.0).max(0.1) * 100.0;
                    log::info!(
                        "under-utilised render detected (last page {:.1}%) — attempting LLM expansion (target +{:.0}%)",
                        su.last_page_utilization_pct,
                        target_pct
                    );

                    match distill::distill_expand(
                        "distilled.md",
                        "distilled.expanded.md",
                        target_pct,
                    )
                    .await
                    {
                        Ok(_) => {
                            log::info!("LLM expansion succeeded — re-rendering");
                            let expanded_render = self.render_pdf(
                                "distilled.expanded.md",
                                output_pdf,
                                template_path,
                                max_pages,
                            );
                            if expanded_render.ok() {
                                if let Some(ref expanded_su) = expanded_render.space_utilization {
                                    if !expanded_su.last_page_under_utilized {
                                        log::info!("expansion resolved under-utilisation");
                                    } else {
                                        log::info!(
                                            "expansion improved but still under-utilised ({:.1}%)",
                                            expanded_su.last_page_utilization_pct
                                        );
                                    }
                                }
                                result.render = expanded_render;
                            } else {
                                log::warn!("re-render after expansion failed — keeping original");
                            }
                        }
                        Err(e) => {
                            log::warn!("LLM expansion failed: {:#} — keeping original", e);
                        }
                    }
                }
            }
        }

        result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // PipelineResult tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pipeline_result_succeeded_is_ok() {
        let r = PipelineResult::succeeded(vec!["a.json".to_string()], vec!["done".to_string()]);
        assert_eq!(r.status, "succeeded");
        assert!(r.ok());
        assert_eq!(r.artifact_paths.len(), 1);
        assert!(r.errors.is_empty());
        assert_eq!(r.logs.len(), 1);
    }

    #[test]
    fn test_pipeline_result_failed_is_not_ok() {
        let r = PipelineResult::failed(vec!["boom".to_string()], vec!["tried".to_string()]);
        assert_eq!(r.status, "failed");
        assert!(!r.ok());
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.errors[0], "boom");
        assert_eq!(r.logs.len(), 1);
    }

    #[test]
    fn test_pipeline_result_skipped() {
        let r = PipelineResult::skipped();
        assert_eq!(r.status, "skipped");
        assert!(!r.ok());
        assert!(r.artifact_paths.is_empty());
        assert!(r.errors.is_empty());
        assert!(r.logs.is_empty());
    }

    #[test]
    fn test_pipeline_result_json_roundtrip() {
        let r = PipelineResult {
            status: "succeeded".to_string(),
            artifact_paths: vec!["a.json".to_string(), "b.srt".to_string()],
            errors: vec![],
            logs: vec!["log1".to_string(), "log2".to_string()],
            space_utilization: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: PipelineResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, "succeeded");
        assert_eq!(parsed.artifact_paths.len(), 2);
        assert_eq!(parsed.logs.len(), 2);
    }

    // -----------------------------------------------------------------------
    // PipelineRunner tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pipeline_runner_new_creates_project_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj_dir = tmp.path().join("my-project");
        assert!(!proj_dir.exists());

        let runner = PipelineRunner::new(proj_dir.to_string_lossy().to_string()).unwrap();
        assert!(runner.project_dir.ends_with("my-project"));
        assert!(Path::new(&runner.project_dir).exists());
    }

    #[test]
    fn test_pipeline_runner_new_accepts_str() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj_dir = tmp.path().join("proj2");
        let proj_str = proj_dir.to_string_lossy().to_string();

        // Pass as &str - `impl Into<String>` must accept both.
        let runner = PipelineRunner::new(proj_str.as_str()).unwrap();
        assert!(Path::new(&runner.project_dir).exists());
    }

    #[test]
    fn test_pipeline_runner_resolve_handles_relative_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj_dir = tmp.path().join("resolve-test");
        let runner = PipelineRunner::new(proj_dir.to_string_lossy().to_string()).unwrap();

        let resolved = runner.resolve("transcripts");
        assert!(resolved.contains("resolve-test"));
        assert!(resolved.contains("transcripts"));
        // Must be an absolute-ish path rooted in the project dir.
        assert!(resolved.contains(&runner.project_dir));
    }

    #[test]
    fn test_pipeline_runner_resolve_handles_nested_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj_dir = tmp.path().join("nested");
        let runner = PipelineRunner::new(proj_dir.to_string_lossy().to_string()).unwrap();

        let resolved = runner.resolve("sub/dir/file.md");
        assert!(resolved.ends_with("file.md"));
        assert!(resolved.contains("sub"));
        assert!(resolved.contains("dir"));
    }

    // -----------------------------------------------------------------------
    // PipelineRunResult tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pipeline_run_result_all_ok_when_all_succeeded() {
        let mut r = PipelineRunResult::new();
        r.fetch = PipelineResult::succeeded(vec![], vec![]);
        r.patch = PipelineResult::succeeded(vec![], vec![]);
        r.distill = PipelineResult::succeeded(vec![], vec![]);
        r.render = PipelineResult::succeeded(vec![], vec![]);
        assert!(r.all_ok());
    }

    #[test]
    fn test_pipeline_run_result_all_ok_when_one_failed() {
        let mut r = PipelineRunResult::new();
        r.fetch = PipelineResult::succeeded(vec![], vec![]);
        r.patch = PipelineResult::failed(vec!["fail".to_string()], vec![]);
        r.distill = PipelineResult::succeeded(vec![], vec![]);
        r.render = PipelineResult::succeeded(vec![], vec![]);
        assert!(!r.all_ok());
    }

    #[test]
    fn test_pipeline_run_result_all_ok_when_one_skipped() {
        // Skipped is not "succeeded" - all_ok requires every stage to be
        // explicitly succeeded.
        let mut r = PipelineRunResult::new();
        r.fetch = PipelineResult::succeeded(vec![], vec![]);
        r.patch = PipelineResult::succeeded(vec![], vec![]);
        r.distill = PipelineResult::skipped();
        r.render = PipelineResult::succeeded(vec![], vec![]);
        assert!(!r.all_ok());
    }

    #[test]
    fn test_pipeline_run_result_new_all_skipped() {
        let r = PipelineRunResult::new();
        assert_eq!(r.fetch.status, "skipped");
        assert_eq!(r.patch.status, "skipped");
        assert_eq!(r.distill.status, "skipped");
        assert_eq!(r.render.status, "skipped");
        assert!(!r.all_ok());
    }

    // -----------------------------------------------------------------------
    // render_pdf panic safety
    // -----------------------------------------------------------------------

    #[test]
    fn test_render_pdf_catches_panics() {
        // Use a non-existent input file with an odd path that causes a panic
        // rather than a clean error.  Even if it only produces an Err, the
        // catch_unwind wrapper must return a "failed" PipelineResult.
        let tmp = tempfile::TempDir::new().unwrap();
        let proj_dir = tmp.path().join("panic-test");
        let runner = PipelineRunner::new(proj_dir.to_string_lossy().to_string()).unwrap();

        let result = runner.render_pdf("nonexistent.md", "out.pdf", None, 2);
        assert_eq!(result.status, "failed");
        assert!(!result.errors.is_empty());
    }

    /// Verify that `render_pdf` with invalid input does not panic - it returns
    /// a `PipelineResult` with status `"failed"` instead.
    #[test]
    fn test_render_pdf_does_not_panic_on_invalid_input() {
        let tmp = tempfile::TempDir::new().unwrap();
        let proj_dir = tmp.path().join("no-panic-test");
        fs::create_dir_all(&proj_dir).unwrap();

        // Create an input file with broken Markdown that could trip up the
        // LaTeX conversion.
        let input = proj_dir.join("broken.md");
        fs::write(&input, "# Title\n\nUnclosed $\\frac{x}{y").unwrap();

        let runner = PipelineRunner::new(proj_dir.to_string_lossy().to_string()).unwrap();
        // This should never panic - catch_unwind guards the call.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runner.render_pdf("broken.md", "out.pdf", None, 2)
        }));
        assert!(result.is_ok(), "render_pdf must not panic");
    }
}
