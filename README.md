# lecture-distill

SJTU Canvas course video subtitle ingestion, exam-focused distillation, and cheat sheet PDF rendering.

**Primary distribution: Rust binary.** A Python reference implementation is available in `lecture_distill/` for comparison but is not the recommended usage path.

## Installation

### Rust (primary)

```bash
# Install via Cargo
cargo install --path .

# Or build from source
cargo build --release
# Binary: ./target/release/lecture-distill[.exe]
```

**Prerequisites:** [Rust](https://rustup.rs/) >= 1.75.

### Python (legacy reference)

```bash
pip install -e .
```

## Prerequisites

- **PDF renderer** for cheat sheet rendering (install one):
  - [Typst](https://typst.app/) (recommended): `winget install --id Typst.Typst`
  - TeX Live/MiKTeX with `xelatex` or `latexmk` as a fallback
- **LLM (optional)**: Set `OPENAI_API_KEY` for AI-powered distillation. Without it, deterministic fallbacks are used.

```bash
# Linux / macOS
export OPENAI_API_KEY=your-api-key
export OPENAI_BASE_URL=https://api.openai.com/v1   # optional
export OPENAI_MODEL=gpt-4o-mini                      # optional

# Windows (PowerShell)
$env:OPENAI_API_KEY="your-api-key"
```

## Quick Start

### 1. List course videos

```bash
lecture-distill canvas list-videos \
  --course-id 12345 \
  --cookie "your-ja-auth-cookie-value"
```

Get your `JAAuthCookie` from browser Developer Tools -> Application -> Cookies on `oc.sjtu.edu.cn`.

### 2. Fetch subtitles

```bash
lecture-distill canvas fetch-subtitles \
  --course-id 12345 \
  --cookie "your-ja-auth-cookie-value" \
  --out data/transcripts
```

This saves each video's transcript as a JSON artifact and SRT file in `data/transcripts/`.

### 3. Patch your notes

```bash
lecture-distill patch-notes \
  --notes notes.md \
  --transcripts data/transcripts \
  --out notes.patched.md \
  --patches patches.json
```

Your original `notes.md` is **never modified**. A new `notes.patched.md` is created with additions and a "Conflicts / Needs Review" section.

### 4. Distill for exam review

```bash
lecture-distill distill \
  --notes notes.patched.md \
  --out distilled.md
```

If an LLM is configured, concepts are scored by exam relevance. Otherwise, headings, formulas, and bold terms are extracted deterministically.

### 5. Render cheat sheet PDF

```bash
lecture-distill render-cheatsheet \
  --input distilled.md \
  --out build/cheatsheet.pdf \
  --max-pages 2
```

Uses Typst first when available, then falls back to LaTeX. If Typst is not on `PATH`, pass an explicit executable path:

```bash
lecture-distill render-cheatsheet \
  --input distilled.md \
  --out build/cheatsheet.pdf \
  --max-pages 2 \
  --typst-path /path/to/typst
```

If the output exceeds the page limit, up to 3 compression attempts are made automatically.

### 6. Full pipeline (one command)

```bash
lecture-distill run \
  --course-id 12345 \
  --cookie "your-ja-auth-cookie-value" \
  --notes notes.md \
  --out build/cheatsheet.pdf
```

## GUI (Web Interface)

A local web GUI is available for interactive control of the pipeline:

```bash
lecture-distill gui --host 127.0.0.1 --port 8765 --project-dir .lecture-distill/projects/default
```

Open http://127.0.0.1:8765 in your browser. The GUI provides:

- **Dashboard**: package version, LLM/PDF renderer availability, project directory, latest artifacts
- **Canvas**: list videos, fetch subtitles (cookie is used in memory only, never saved)
- **Notes**: patch notes with transcripts, preview results
- **Distill**: run distillation from patched notes
- **Cheat Sheet**: render PDF with page count feedback or clear renderer errors
- **Logs**: view running and completed jobs with status and output

The GUI is server-rendered with no separate frontend build step.

## Commands

| Command | Description |
|---|---|
| `canvas list-videos` | List available videos in a Canvas course |
| `canvas fetch-subtitles` | Fetch subtitles for all/specific videos |
| `patch-notes` | Patch Markdown notes with transcript content |
| `distill` | Distill notes into exam-focused summary |
| `render-cheatsheet` | Render Markdown as cheat sheet PDF |
| `run` | Full end-to-end pipeline |
| `gui` | Start local Web GUI |
| `--version` | Print version |

## Custom Cheat Sheet Templates

Create a `.typ` or `.tex` file with a `{{content}}` placeholder, then pass it with `--template`:

```bash
lecture-distill render-cheatsheet \
  --input distilled.md \
  --template my_template.tex \
  --out cheat_sheet.pdf
```

Typst templates are used by the Typst renderer. LaTeX templates are used by the LaTeX fallback.

## Architecture

```
src/
  main.rs           # CLI entrypoint (clap)
  artifacts.rs      # Data models (serde)
  canvas_sjtu.rs    # SJTU Canvas video connector
  transcripts.rs    # SRT parsing and serialization
  notes.rs          # Markdown notes patching
  llm.rs            # OpenAI-compatible LLM integration
  ranking.rs        # Exam relevance scoring
  distill.rs        # Distillation workflow
  latex.rs          # Typst/LaTeX rendering and PDF compilation
  pipeline.rs       # Shared PipelineRunner for CLI and Web
  web/
    mod.rs          # Web module root
    app.rs          # Axum app factory and routes
    state.rs        # JSON-backed project state (no secrets)
    jobs.rs         # In-memory background job registry
```

The legacy Python reference implementation lives under `lecture_distill/` and is ignored by Git.

## Development

```bash
# Build
cargo build

# Run tests
cargo test

# Format
cargo fmt --check

# Build release
cargo build --release

# Run CLI help
cargo run -- --help
```

## Validation

```bash
cargo fmt --check
cargo test
cargo build --release
cargo run -- --help
cargo run -- gui --help
```

## Limitations (MVP)

- Only the SJTU Canvas video flow is supported (not generic Canvas/Panopto)
- Videos are not downloaded; only subtitles/transcripts are fetched
- PDF rendering requires Typst or a local LaTeX installation

## License

MIT
