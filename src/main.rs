//! lecture-distill - SJTU Canvas video subtitle ingestion, Markdown notes
//! patching, exam-focused distillation, and cheat sheet PDF rendering.
//!
//! # Commands
//!
//! ```text
//! lecture-distill canvas list-videos     --course-id <ID> --cookie <COOKIE>
//! lecture-distill canvas fetch-subtitles --course-id <ID> --cookie <COOKIE> --out <DIR> [--video-id <ID>]
//! lecture-distill patch-notes            --notes <FILE> --transcripts <DIR> --out <FILE> --patches <FILE>
//! lecture-distill distill                --notes <FILE> --out <FILE>
//! lecture-distill render-cheatsheet      --input <FILE> --out <FILE> [--template <FILE>] [--max-pages <N>] [--typst-path <PATH>]
//! lecture-distill run                    --course-id <ID> --cookie <COOKIE> --notes <FILE> --out <FILE> [--max-pages <N>]
//! lecture-distill gui                    [--host <HOST>] [--port <PORT>] [--project-dir <DIR>]
//! lecture-distill version
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};
use lecture_distill::*;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// SJTU Canvas lecture video subtitle ingestion and exam-focused distillation.
#[derive(Parser)]
#[command(name = "lecture-distill", version = env!("CARGO_PKG_VERSION"), about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Canvas video operations
    Canvas {
        #[command(subcommand)]
        cmd: CanvasCmd,
    },
    /// Patch lecture notes with transcript content
    PatchNotes {
        /// Path to source Markdown notes
        #[arg(short, long)]
        notes: PathBuf,
        /// Directory containing transcript JSON files
        #[arg(short, long)]
        transcripts: PathBuf,
        /// Output path for patched notes
        #[arg(long, default_value = "notes.patched.md")]
        out: PathBuf,
        /// Output path for patches JSON artifact
        #[arg(long, default_value = "patches.json")]
        patches: PathBuf,
    },
    /// Distill notes into an exam-focused summary
    Distill {
        /// Path to (patched) Markdown notes
        #[arg(short, long)]
        notes: PathBuf,
        /// Output path for distilled notes
        #[arg(long, default_value = "distilled.md")]
        out: PathBuf,
    },
    /// Render a Markdown file as a compact cheat sheet PDF
    RenderCheatsheet {
        /// Path to distilled Markdown input
        #[arg(long = "input")]
        input_md: PathBuf,
        /// Path to template file (uses default if omitted; .typ for Typst, .tex for LaTeX)
        #[arg(long = "template")]
        template: Option<PathBuf>,
        /// Path to Typst executable (uses PATH if omitted)
        #[arg(long = "typst-path")]
        typst_path: Option<PathBuf>,
        /// Output PDF path
        #[arg(long, default_value = "build/cheatsheet.pdf")]
        out: PathBuf,
        /// Maximum page count
        #[arg(long = "max-pages", default_value = "2")]
        max_pages: usize,
    },
    /// Full pipeline: fetch -> patch -> distill -> render cheat sheet
    Run {
        /// Canvas course ID
        #[arg(long = "course-id")]
        course_id: String,
        /// JAAuthCookie value from browser
        #[arg(long)]
        cookie: String,
        /// Path to source Markdown notes
        #[arg(long)]
        notes: PathBuf,
        /// Path to template file (uses default if omitted; .typ for Typst, .tex for LaTeX)
        #[arg(long)]
        template: Option<PathBuf>,
        /// Path to Typst executable (uses PATH if omitted)
        #[arg(long = "typst-path")]
        typst_path: Option<PathBuf>,
        /// Output PDF path
        #[arg(long, default_value = "build/cheatsheet.pdf")]
        out: PathBuf,
        /// Maximum page count
        #[arg(long = "max-pages", default_value = "2")]
        max_pages: usize,
        /// Transcript cache directory
        #[arg(long = "transcripts-dir", default_value = "data/transcripts")]
        transcripts_dir: PathBuf,
    },
    /// Run processing operations (same as GUI processing)
    Process {
        #[command(subcommand)]
        cmd: ProcessCmd,
    },
    /// Calibrate budget constants for a Typst template
    Calibrate {
        /// Path to Typst template file (uses embedded default if omitted)
        #[arg(long = "template")]
        template: Option<PathBuf>,
        /// Force re-calibration even if cached data exists
        #[arg(long = "force")]
        force: bool,
        /// Project directory for calibration cache
        #[arg(
            long = "project-dir",
            default_value = ".lecture-distill/projects/default"
        )]
        project_dir: PathBuf,
    },
    /// Start the local Web GUI
    Gui {
        /// Host to bind the web server
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind the web server
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Project directory for state and artifacts
        #[arg(
            long = "project-dir",
            default_value = ".lecture-distill/projects/default"
        )]
        project_dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum ProcessCmd {
    /// Run processing on sources in a project directory
    Run {
        /// Project directory containing sources and config
        #[arg(
            long = "project-dir",
            default_value = ".lecture-distill/projects/default"
        )]
        project_dir: PathBuf,
        /// Comma-separated output kinds with optional max pages
        /// (e.g. "note_patch,reference_digest,cheating_sheet:2")
        #[arg(long = "outputs")]
        outputs: Option<String>,
        /// Enable debug diagnostics and keep intermediate files
        #[arg(long = "debug", default_value_t = false)]
        debug: bool,
        /// Re-run an existing process by ID (uses its saved source_ids and outputs)
        #[arg(long = "process-id")]
        process_id: Option<String>,
        /// Comma-separated source IDs to process (default: all sources)
        #[arg(long = "source-ids")]
        source_ids: Option<String>,
    },
}

#[derive(Subcommand)]
enum CanvasCmd {
    /// List available videos for a Canvas course
    ListVideos {
        /// Canvas course ID
        #[arg(long = "course-id")]
        course_id: String,
        /// JAAuthCookie value from browser
        #[arg(long)]
        cookie: String,
    },
    /// Fetch subtitles/transcripts from Canvas course videos
    FetchSubtitles {
        /// Canvas course ID
        #[arg(long = "course-id")]
        course_id: String,
        /// JAAuthCookie value from browser
        #[arg(long)]
        cookie: String,
        /// Output directory for transcript files
        #[arg(long, default_value = "data/transcripts")]
        out: PathBuf,
        /// Specific video ID (fetches all if omitted)
        #[arg(long = "video-id")]
        video_id: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

/// Open a URL in the default system browser.
fn open_browser(url: &str) {
    let result = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    match result {
        Ok(_) => log::info!("Browser opened: {}", url),
        Err(e) => log::warn!("Failed to open browser ({}): {}", url, e),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // When launched with no arguments (e.g. double-clicking the .exe on Windows),
    // automatically start the GUI and open the browser.
    let args: Vec<String> = std::env::args().collect();
    if args.len() <= 1 {
        let host = "127.0.0.1";
        let port: u16 = 8765;
        let project_dir = ".lecture-distill/projects/default";

        let _ = env_logger::try_init();

        println!("========================================");
        println!("  lecture-distill GUI v{}", env!("CARGO_PKG_VERSION"));
        println!("========================================");
        println!("  Starting at http://{}:{}", host, port);
        println!("  Project dir: {}", project_dir);
        println!("  Press Ctrl+C to stop.");
        println!();

        let url = format!("http://{}:{}", host, port);

        // Open browser after a short delay to let the server start.
        let browser_url = url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            open_browser(&browser_url);
        });

        let app = web::app::create_app(project_dir);
        let listener = tokio::net::TcpListener::bind(&format!("{}:{}", host, port)).await?;
        println!("Listening on http://{}:{}\n", host, port);

        axum::serve(listener, app).await?;
        return Ok(());
    }

    let cli = Cli::parse();

    match cli.command {
        Commands::Canvas { cmd } => match cmd {
            CanvasCmd::ListVideos { course_id, cookie } => {
                cmd_list_videos(&course_id, &cookie).await?;
            }
            CanvasCmd::FetchSubtitles {
                course_id,
                cookie,
                out,
                video_id,
            } => {
                cmd_fetch_subtitles(&course_id, &cookie, &out, video_id.as_deref()).await?;
            }
        },

        Commands::PatchNotes {
            notes,
            transcripts,
            out,
            patches,
        } => {
            cmd_patch_notes(&notes, &transcripts, &out, &patches).await?;
        }

        Commands::Distill { notes, out } => {
            cmd_distill(&notes, &out).await?;
        }

        Commands::RenderCheatsheet {
            input_md,
            template,
            typst_path,
            out,
            max_pages,
        } => {
            cmd_render_cheatsheet(
                &input_md,
                template.as_deref(),
                typst_path.as_deref(),
                &out,
                max_pages,
            )?;
        }

        Commands::Run {
            course_id,
            cookie,
            notes,
            template,
            typst_path,
            out,
            max_pages,
            transcripts_dir,
        } => {
            cmd_run(
                &course_id,
                &cookie,
                &notes,
                template.as_deref(),
                typst_path.as_deref(),
                &out,
                max_pages,
                &transcripts_dir,
            )
            .await?;
        }

        Commands::Process { cmd } => match cmd {
            ProcessCmd::Run {
                project_dir,
                outputs,
                debug,
                process_id,
                source_ids,
            } => {
                cmd_process_run(
                    &project_dir,
                    outputs.as_deref(),
                    debug,
                    process_id.as_deref(),
                    source_ids.as_deref(),
                )
                .await?;
            }
        },

        Commands::Calibrate {
            template,
            force,
            project_dir,
        } => {
            cmd_calibrate(template.as_deref(), force, &project_dir)?;
        }

        Commands::Gui {
            host,
            port,
            project_dir,
        } => {
            cmd_gui(&host, port, &project_dir).await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

/// `canvas list-videos`
async fn cmd_list_videos(course_id: &str, cookie: &str) -> Result<()> {
    let mut client =
        canvas_sjtu::CanvasSJTUVideoClient::new(course_id.to_string(), cookie.to_string());

    println!("Authenticating with Canvas course {}...", course_id);
    client.authenticate().await?;
    println!("Authentication successful.\n");

    let videos = client.list_videos().await?;

    if videos.is_empty() {
        println!("No videos found for this course.");
        return Ok(());
    }

    println!("Videos for course {}:\n", course_id);
    println!("{:<40} {:<50} {}", "Video ID", "Title", "Duration");
    println!("{}", "-".repeat(100));
    for v in &videos {
        let mins = v.duration / 60;
        let secs = v.duration % 60;
        println!("{:<40} {:<50} {}:{:02}", v.video_id, v.title, mins, secs);
    }
    println!("\nTotal: {} video(s)", videos.len());

    Ok(())
}

/// `canvas fetch-subtitles`
async fn cmd_fetch_subtitles(
    course_id: &str,
    cookie: &str,
    out_dir: &PathBuf,
    video_id: Option<&str>,
) -> Result<()> {
    use transcripts::transcript_to_srt;

    std::fs::create_dir_all(out_dir)?;

    let mut client =
        canvas_sjtu::CanvasSJTUVideoClient::new(course_id.to_string(), cookie.to_string());

    println!("Authenticating...");
    client.authenticate().await?;
    println!("Authentication successful.\n");

    let artifacts = if let Some(vid) = video_id {
        println!("Fetching video: {}", vid);
        vec![client.fetch_subtitles(vid).await?]
    } else {
        let videos = client.list_videos().await?;
        println!("Found {} videos. Fetching subtitles...", videos.len());

        let mut collected = Vec::new();
        for v in &videos {
            match client.fetch_subtitles(&v.video_id).await {
                Ok(art) => collected.push(art),
                Err(e) => eprintln!("  SKIP {}: {}", v.video_id, e),
            }
        }
        collected
    };

    for art in &artifacts {
        // Save JSON artifact.
        let json_path = out_dir.join(format!("{}.json", art.video_id));
        let json = serde_json::to_string_pretty(art)?;
        std::fs::write(&json_path, &json)?;
        println!("  JSON -> {}", json_path.display());

        // Save SRT file.
        let srt_path = out_dir.join(format!("{}.srt", art.video_id));
        let srt_text = transcript_to_srt(art);
        std::fs::write(&srt_path, &srt_text)?;
        println!("  SRT  -> {}", srt_path.display());
    }

    println!(
        "\nSaved {} transcript(s) to {}/",
        artifacts.len(),
        out_dir.display()
    );

    Ok(())
}

/// `patch-notes`
async fn cmd_patch_notes(
    notes_path: &PathBuf,
    transcripts_dir: &PathBuf,
    out: &PathBuf,
    patches: &PathBuf,
) -> Result<()> {
    println!("Patching notes: {}", notes_path.display());
    println!("Transcripts directory: {}", transcripts_dir.display());

    notes::patch_notes(
        &notes_path.to_string_lossy(),
        &transcripts_dir.to_string_lossy(),
        &out.to_string_lossy(),
        &patches.to_string_lossy(),
    )
    .await?;

    println!("Patched notes -> {}", out.display());
    println!("Patches JSON -> {}", patches.display());

    Ok(())
}

/// `distill`
async fn cmd_distill(notes_path: &PathBuf, out: &PathBuf) -> Result<()> {
    println!("Distilling: {}", notes_path.display());

    let distilled = distill::distill(&notes_path.to_string_lossy(), &out.to_string_lossy()).await?;

    let lines = distilled.lines().count();
    let chars = distilled.len();
    println!("Distilled -> {}", out.display());
    println!("Output: {} lines, {} characters", lines, chars);

    Ok(())
}

/// `render-cheatsheet`
fn cmd_render_cheatsheet(
    input_md: &PathBuf,
    template: Option<&std::path::Path>,
    typst_path: Option<&std::path::Path>,
    out: &PathBuf,
    max_pages: usize,
) -> Result<()> {
    println!("Rendering: {}", input_md.display());
    println!("Max pages: {}", max_pages);

    if let Some(path) = typst_path {
        std::env::set_var(
            "LECTURE_DISTILL_TYPST_PATH",
            path.to_string_lossy().to_string(),
        );
        println!("Typst path: {}", path.display());
    } else if std::env::var("LECTURE_DISTILL_TYPST_PATH").is_err() {
        // Auto-detect from common locations when not explicitly provided.
        // Check if typst is on PATH.
        let typst = crate::latex::find_typst_compiler();
        if !typst.is_empty() {
            std::env::set_var("LECTURE_DISTILL_TYPST_PATH", &typst);
            println!("Typst auto-detected: {}", typst);
        }
    }

    let tp = template.map(|p| p.to_string_lossy().to_string());
    let artifact = latex::render_cheatsheet(
        &input_md.to_string_lossy(),
        tp.as_deref(),
        &out.to_string_lossy(),
        max_pages,
    )?;

    println!(
        "Cheat sheet: {} ({} page(s))",
        artifact.pdf_path, artifact.page_count
    );
    println!("Template used: {}", artifact.template_used);
    if artifact.compression_attempts > 0 {
        println!("Compression attempts: {}", artifact.compression_attempts);
    }

    Ok(())
}

/// `run` - full end-to-end pipeline.
async fn cmd_run(
    course_id: &str,
    cookie: &str,
    notes_path: &PathBuf,
    template: Option<&std::path::Path>,
    typst_path: Option<&std::path::Path>,
    out_pdf: &PathBuf,
    max_pages: usize,
    transcripts_dir: &PathBuf,
) -> Result<()> {
    println!("========================================");
    println!("  lecture-distill - Full Pipeline");
    println!("========================================\n");

    // Step 1: Fetch transcripts.
    println!("[Step 1/4] Fetching transcripts...");
    cmd_fetch_subtitles(course_id, cookie, transcripts_dir, None).await?;
    println!();

    // Step 2: Patch notes.
    println!("[Step 2/4] Patching notes...");
    let patched_md = PathBuf::from("notes.patched.md");
    let patches_json = PathBuf::from("patches.json");
    cmd_patch_notes(notes_path, transcripts_dir, &patched_md, &patches_json).await?;
    println!();

    // Step 3: Distill.
    println!("[Step 3/4] Distilling notes...");
    let distilled_md = PathBuf::from("distilled.md");
    cmd_distill(&patched_md, &distilled_md).await?;
    println!();

    // Step 4: Render cheat sheet.
    println!("[Step 4/4] Rendering cheat sheet...");
    cmd_render_cheatsheet(&distilled_md, template, typst_path, out_pdf, max_pages)?;
    println!();

    println!("Done! Cheat sheet: {}", out_pdf.display());

    Ok(())
}

/// `process run` - execute processing operations on sources (same as GUI processing).
///
/// When `process_id` is provided, re-runs an existing process in-place using its
/// saved source_ids and outputs.  When omitted, creates a new process.
async fn cmd_process_run(
    project_dir: &PathBuf,
    outputs: Option<&str>,
    debug: bool,
    process_id_override: Option<&str>,
    source_ids_filter: Option<&str>,
) -> Result<()> {
    use std::sync::Arc;
    use utils::output::{
        cheating_sheet_markdown_path, expand_output_kinds, process_output_path_for,
        process_output_title, update_process_terminal_status, CreateProcessOutputBody,
    };
    use web::processes::{
        ProcessOutput, ProcessOutputKind, ProcessRecord, ProcessStatus as ProcessRecordStatus,
        ProcessStore,
    };
    use web::sources::SourceStore;

    // Initialize logger: debug mode shows info, normal mode shows warnings only.
    if debug {
        std::env::set_var("RUST_LOG", "info");
        std::env::set_var("LECTURE_DISTILL_DEBUG", "1");
    } else {
        std::env::set_var("RUST_LOG", "warn");
    }
    let _ = env_logger::try_init();

    let project_dir_str = project_dir.to_string_lossy().to_string();

    // Load project config (typst_path, etc.) — mirrors the web app startup.
    // The web app reads config.json and sets LECTURE_DISTILL_TYPST_PATH etc.
    // We do the same here so the CLI behaves identically.
    {
        let config_path = std::path::Path::new(&project_dir_str).join("config.json");
        if config_path.exists() {
            if let Ok(config_json) = std::fs::read_to_string(&config_path) {
                if let Ok(config) = serde_json::from_str::<serde_json::Value>(&config_json) {
                    if let Some(tp) = config.get("typst_path").and_then(|v| v.as_str()) {
                        if !tp.trim().is_empty() {
                            std::env::set_var("LECTURE_DISTILL_TYPST_PATH", tp.trim());
                            println!("[config] typst_path: {}", tp.trim());
                        }
                    }
                }
            }
        }
    }

    // Load secrets (API keys) from secrets.local.json — mirrors GUI path.
    {
        let secret_store = crate::web::secrets::SecretStore::new(&project_dir_str);
        let secrets = secret_store.load();
        secrets.apply_to_env();
        if std::env::var("OPENAI_API_KEY").is_ok() {
            println!("[secrets] LLM API key loaded from secrets.local.json");
        } else {
            println!("[secrets] No API key found — LLM features may be unavailable");
        }
    }

    // 1. Initialize stores (same pattern as create_app()).
    let source_store = Arc::new(SourceStore::new(&project_dir_str));
    let process_store = Arc::new(ProcessStore::new(&project_dir_str));

    // ── Re-run existing process path ──
    if let Some(pid) = process_id_override {
        let existing = match process_store.get(pid) {
            Some(p) => p,
            None => anyhow::bail!(
                "Process '{}' not found in project: {}\n\
                 Use 'process run' without --process-id to create a new process.",
                pid,
                project_dir.display()
            ),
        };

        println!("═══ Re-running existing process ═══");
        println!("Process ID: {}", pid);
        println!("Title: {}", existing.title);
        println!("Sources: {}", existing.source_ids.len());
        println!("Outputs: {}", existing.outputs.len());
        for o in &existing.outputs {
            println!(
                "  - [{}] {} ({})",
                match o.status {
                    ProcessRecordStatus::Ready => "✓",
                    ProcessRecordStatus::Failed => "✗",
                    ProcessRecordStatus::Processing => "…",
                },
                o.title,
                o.kind,
            );
            if o.kind == ProcessOutputKind::CheatingSheet {
                if let Some(mp) = o.metadata.get("max_pages").and_then(|v| v.as_u64()) {
                    println!("      max_pages: {}", mp);
                }
            }
        }
        println!();

        // Reset all outputs to Processing state.
        let now = ProcessRecord::now_iso();
        let _ = process_store.update(pid, |r| {
            r.status = ProcessRecordStatus::Processing;
            r.last_error = None;
            for o in &mut r.outputs {
                o.status = ProcessRecordStatus::Processing;
                o.last_error = None;
                o.updated_at = now.clone();
            }
        });

        let process = process_store.get(pid).unwrap();
        let source_ids = process.source_ids.clone();
        let outputs_vec = process.outputs.clone();

        println!("Processing...\n");
        pipelines::run_process_outputs(
            pid,
            &source_ids,
            &outputs_vec,
            &process_store,
            &source_store,
            "cli",
        )
        .await;

        update_process_terminal_status(&process_store, pid, "cli");

        // Print results.
        print_process_results(&process_store, pid, project_dir_str, debug);
        return Ok(());
    }

    // ── New process path (original behaviour) ──

    // 2. Load and validate sources.
    let sources = source_store.load_all();
    if sources.is_empty() {
        anyhow::bail!(
            "No sources found in project directory: {}\n\
             Use the GUI or 'canvas fetch-subtitles' to add sources first.",
            project_dir.display()
        );
    }

    // Collect all source IDs (CLI processes all sources in the project).
    let mut source_ids: Vec<String> = sources.iter().map(|s| s.id.clone()).collect();

    // Apply source filter if provided.
    if let Some(filter_str) = source_ids_filter {
        let filter_set: std::collections::HashSet<&str> = filter_str
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !filter_set.is_empty() {
            let before = source_ids.len();
            source_ids.retain(|id| filter_set.contains(id.as_str()));
            println!(
                "Source filter: {} of {} sources selected",
                source_ids.len(),
                before
            );
            if source_ids.is_empty() {
                anyhow::bail!(
                    "No sources matched the filter. Available source IDs:\n{}",
                    sources
                        .iter()
                        .map(|s| format!("  {} — {}", s.id, s.title))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    }

    println!("Project: {}", project_dir.display());
    println!("Sources: {} source(s)", source_ids.len());
    for s in &sources {
        println!("  - [{}] {} ({})", s.kind, s.title, s.id);
    }
    println!();

    // 3. Parse --outputs into CreateProcessOutputBody items.
    let outputs_str = outputs.unwrap_or("");
    let requested: Vec<CreateProcessOutputBody> = outputs_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|token| {
            if let Some((kind, pages)) = token.split_once(':') {
                let max_pages: usize = pages
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid max_pages in '{}'", token))?;
                Ok(CreateProcessOutputBody {
                    kind: kind.trim().to_string(),
                    max_pages: Some(max_pages),
                })
            } else {
                Ok(CreateProcessOutputBody {
                    kind: token.to_string(),
                    max_pages: None,
                })
            }
        })
        .collect::<Result<Vec<_>>>()?;

    if requested.is_empty() {
        anyhow::bail!(
            "At least one output kind is required. Supported: note_patch, reference_digest, cheating_sheet[:N]"
        );
    }

    // 4. Expand output kinds (auto-add dependencies like RefDigest for CheatSheet).
    let expanded = match expand_output_kinds(&requested) {
        Ok(kinds) => kinds,
        Err(e) => anyhow::bail!("{}", e),
    };

    println!("Outputs requested:");
    for (kind, max_pages) in &expanded {
        if *max_pages > 0 {
            println!(
                "  - {} (max_pages: {})",
                process_output_title(kind),
                max_pages
            );
        } else {
            println!("  - {}", process_output_title(kind));
        }
    }
    println!();

    // 5. Create ProcessRecord + ProcessOutput[].
    let process_id = uuid::Uuid::new_v4().to_string();
    let now = ProcessRecord::now_iso();
    let title = format!("CLI Process {}", &process_id[..8]);

    let mut outputs_vec: Vec<ProcessOutput> = Vec::new();
    for (kind, max_pages) in &expanded {
        let output_id = uuid::Uuid::new_v4().to_string();
        let output_path = process_output_path_for(&process_store, &process_id, &output_id, kind);
        let diff_path = if *kind == ProcessOutputKind::NotePatch {
            Some(process_store.diff_path(&process_id, &output_id))
        } else {
            None
        };

        // Ensure artifact directories exist.
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let markdown_path = cheating_sheet_markdown_path(&process_store, &process_id, &output_id);

        let metadata = if *kind == ProcessOutputKind::ReferenceDigest {
            serde_json::json!({
                "progress_current": 0,
                "progress_total": 2,
                "progress_label": "queued",
            })
        } else if *kind == ProcessOutputKind::CheatingSheet {
            serde_json::json!({
                "progress_current": 0,
                "progress_total": 4,
                "progress_label": "queued",
                "max_pages": max_pages,
                "markdown_path": markdown_path.to_string_lossy().to_string(),
            })
        } else {
            serde_json::json!({
                "progress_current": 0,
                "progress_total": 1,
                "progress_label": "queued",
            })
        };

        outputs_vec.push(ProcessOutput {
            id: output_id,
            kind: kind.clone(),
            plugin_id: kind.plugin_id().to_string(),
            node_id: kind.node_id().to_string(),
            status: ProcessRecordStatus::Processing,
            title: process_output_title(kind).to_string(),
            path: output_path.to_string_lossy().to_string(),
            diff_path: diff_path.map(|p| p.to_string_lossy().to_string()),
            base_source_id: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_error: None,
            metadata,
        });
    }

    let record = ProcessRecord {
        id: process_id.clone(),
        title,
        status: ProcessRecordStatus::Processing,
        created_at: now.clone(),
        updated_at: now.clone(),
        source_ids: source_ids.clone(),
        outputs: outputs_vec.clone(),
        last_error: None,
        job_id: None,
    };

    process_store.insert(record)?;

    // 6. Execute processing directly (no background job — CLI is already async).
    println!("Processing...\n");
    pipelines::run_process_outputs(
        &process_id,
        &source_ids,
        &outputs_vec,
        &process_store,
        &source_store,
        "cli",
    )
    .await;

    update_process_terminal_status(&process_store, &process_id, "cli");

    print_process_results(&process_store, &process_id, project_dir_str, debug);
    Ok(())
}

/// Print process results (shared between new and re-run paths).
fn print_process_results(
    process_store: &crate::web::processes::ProcessStore,
    process_id: &str,
    project_dir_str: String,
    debug: bool,
) {
    use crate::web::processes::{ProcessOutputKind, ProcessStatus as ProcessRecordStatus};

    // 7. Read results and print.
    let process = process_store.get(&process_id).unwrap();
    println!();
    println!("═══════════════════════════════════════════");
    println!("  Results");
    println!("═══════════════════════════════════════════");
    for output in &process.outputs {
        let status_icon = match output.status {
            ProcessRecordStatus::Ready => "✓",
            ProcessRecordStatus::Failed => "✗",
            ProcessRecordStatus::Processing => "…",
        };
        println!("  [{}] {} ({})", status_icon, output.title, output.status);
        println!("       Path: {}", output.path);

        if output.status == ProcessRecordStatus::Failed {
            if let Some(ref err) = output.last_error {
                println!("       Error: {}", err);
            }
        }

        if output.status == ProcessRecordStatus::Ready {
            // Show output file size.
            if let Ok(meta) = std::fs::metadata(&output.path) {
                println!("       Size: {} bytes", meta.len());
            }

            // Show generated chars from metadata.
            if let Some(generated) = output
                .metadata
                .get("generated_chars")
                .and_then(|v| v.as_u64())
            {
                print!("       Generated chars: {}", generated);
                if let Some(target) = output.metadata.get("target_chars").and_then(|v| v.as_u64()) {
                    let pct = if target > 0 {
                        (generated as f64 / target as f64) * 100.0
                    } else {
                        0.0
                    };
                    println!(" / {} ({:.0}%)", target, pct);
                } else {
                    println!();
                }
            }

            // For CheatSheet, read the rendered artifact for diagnostics.
            if output.kind == ProcessOutputKind::CheatingSheet {
                if let Some(fill) = output.metadata.get("fill_pct").and_then(|v| v.as_str()) {
                    println!("       Typst last page fill: {}%", fill);
                }
            }
        }

        if debug && output.status == ProcessRecordStatus::Ready {
            // ── Enhanced diagnostics ──

            // Char budget analysis
            if let Some(target) = output.metadata.get("target_chars").and_then(|v| v.as_u64()) {
                let generated = output
                    .metadata
                    .get("generated_chars")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let gap_pct = if target > 0 {
                    (generated as f64 / target as f64) * 100.0
                } else {
                    0.0
                };
                let status = if gap_pct >= 90.0 {
                    "OK"
                } else if gap_pct >= 70.0 {
                    "LOW"
                } else {
                    "CRITICAL"
                };
                println!(
                    "       Char budget: {}/{} ({:.0}%) [{}]",
                    generated, target, gap_pct, status
                );
            }

            // Typst fill and utilization
            if let Some(fill) = output.metadata.get("fill_pct").and_then(|v| v.as_str()) {
                println!("       Typst last page fill: {}%", fill);
            }
            if let Some(total_pages) = output
                .metadata
                .get("total_content_pages")
                .and_then(|v| v.as_str())
            {
                println!("       Total content pages: {}", total_pages);
            }

            // Expansion diagnostics
            if let Some(expanded) = output
                .metadata
                .get("expansion_used")
                .and_then(|v| v.as_bool())
            {
                println!("       Expansion used: {}", expanded);
            }
            if let Some(reason) = output
                .metadata
                .get("underfilled_reason")
                .and_then(|v| v.as_str())
            {
                println!("       Underfilled reason: {}", reason);
            }
            if let Some(harness) = output
                .metadata
                .get("harness_attempts")
                .and_then(|v| v.as_u64())
            {
                println!("       LLM harness attempts: {}", harness);
            }

            // Per-page utilization breakdown
            if let Some(pages) = output
                .metadata
                .get("page_utilizations")
                .and_then(|v| v.as_array())
            {
                if !pages.is_empty() {
                    println!("       Per-page utilization:");
                    for p in pages {
                        if let (Some(page), Some(util)) = (
                            p.get("page").and_then(|v| v.as_u64()),
                            p.get("utilization_pct").and_then(|v| v.as_str()),
                        ) {
                            let pct: f64 = util.parse().unwrap_or(0.0);
                            let bar = if pct >= 80.0 {
                                "████"
                            } else if pct >= 60.0 {
                                "███░"
                            } else if pct >= 40.0 {
                                "██░░"
                            } else {
                                "█░░░"
                            };
                            println!("         page {}: {}% {}", page, util, bar);
                        }
                    }
                }
            }

            // For CheatSheet outputs, read markdown for char count.
            if output.kind == ProcessOutputKind::CheatingSheet {
                let markdown_path = output
                    .metadata
                    .get("markdown_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !markdown_path.is_empty() {
                    if let Ok(md_content) = std::fs::read_to_string(markdown_path) {
                        let char_count = md_content.chars().count();
                        // Count PDF page count if possible.
                        if let Ok(pdf_bytes) = std::fs::read(&output.path) {
                            let pdf_text = String::from_utf8_lossy(&pdf_bytes);
                            let page_count = pdf_text.matches("/Type /Page").count()
                                + pdf_text.matches("/Type/Page").count();
                            let page_count = if page_count == 0 { 1usize } else { page_count };
                            let chars_per_page = char_count as f64 / page_count as f64;
                            println!(
                                "       PDF: {} chars, {} pages, {:.0} chars/page",
                                char_count, page_count, chars_per_page
                            );
                        }
                    }
                }
            }

            // Ref Digest diagnostics
            if output.kind == ProcessOutputKind::ReferenceDigest {
                if let Some(generated) = output
                    .metadata
                    .get("generated_chars")
                    .and_then(|v| v.as_u64())
                {
                    println!("       Ref Digest chars: {}", generated);
                }
            }

            if let Some(max_pages) = output.metadata.get("max_pages").and_then(|v| v.as_u64()) {
                println!("       Max pages: {}", max_pages);
            }
        }

        println!();
    }

    println!("Process ID: {}", process_id);
    println!(
        "Artifacts directory: {}/artifacts/processes/{}",
        project_dir_str, process_id
    );
}

/// `calibrate` - run template calibration.
fn cmd_calibrate(
    template: Option<&std::path::Path>,
    force: bool,
    project_dir: &PathBuf,
) -> Result<()> {
    if force {
        // Remove existing calibration file to force re-run.
        match template {
            Some(tp) => {
                let mut cal_path = tp.to_path_buf();
                cal_path.set_extension("calibration.json");
                if cal_path.exists() {
                    std::fs::remove_file(&cal_path)?;
                    println!("Removed existing calibration: {}", cal_path.display());
                }
            }
            None => {
                let def_cal = project_dir
                    .join("calibrations")
                    .join("__default__.calibration.json");
                if def_cal.exists() {
                    std::fs::remove_file(&def_cal)?;
                    println!("Removed existing calibration: {}", def_cal.display());
                }
            }
        }
    }

    let calib = lecture_distill::utils::calibration::ensure_calibration(template, project_dir);

    println!("Template: {}", calib.template);
    println!("Calibrated at: {}", calib.calibrated_at);
    println!("CJK chars/page: {}", calib.cjk_chars_per_page);
    println!("English words/page: {}", calib.english_words_per_page);
    println!("English chars/page: {}", calib.english_chars_per_page);
    println!(
        "Page dimensions: {:.1}mm x {:.1}mm",
        calib.page_width_mm, calib.page_height_mm
    );

    Ok(())
}

/// `gui` - start the local Web GUI.
async fn cmd_gui(host: &str, port: u16, project_dir: &PathBuf) -> Result<()> {
    let project_dir = project_dir.to_string_lossy().to_string();

    println!("========================================");
    println!("  lecture-distill GUI v{}", env!("CARGO_PKG_VERSION"));
    println!("========================================");
    println!("  Starting at http://{}:{}", host, port);
    println!("  Project dir: {}", project_dir);
    println!("  Press Ctrl+C to stop.");
    println!();

    let app = web::app::create_app(&project_dir);
    let addr = format!("{}:{}", host, port);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("Listening on http://{}\n", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
