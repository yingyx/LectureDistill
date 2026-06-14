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

#[tokio::main]
async fn main() -> Result<()> {
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
