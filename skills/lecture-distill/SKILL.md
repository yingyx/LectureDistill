# lecture-distill Skill

## When to use

Use this skill when the user asks to:
- Fetch subtitles or transcripts from SJTU Canvas course videos
- Generate exam cheat sheets from lecture notes
- Distill or summarize lecture notes for exam preparation
- Patch or augment existing Markdown notes with lecture transcript content
- Process SJTU Canvas course materials for exam review
- Launch the local Web GUI for interactive pipeline control

## How to use

This skill is a thin wrapper around the `lecture-distill` **Rust CLI binary**. The primary distribution is a Cargo-built binary. A Python reference implementation is available under `lecture_distill/` as legacy.

### Prerequisites

The `lecture-distill` binary must be compiled:

```bash
cargo build --release
# Binary: ./target/release/lecture-distill[.exe]
```

Or install globally:

```bash
cargo install --path .
```

Set up environment variables for LLM features (optional but recommended):

```bash
# Linux / macOS
export OPENAI_API_KEY=your-api-key
export OPENAI_BASE_URL=https://api.openai.com/v1  # optional
export OPENAI_MODEL=gpt-4o-mini                     # optional

# Windows (PowerShell)
$env:OPENAI_API_KEY="your-api-key"
```

A LaTeX compiler is required for PDF rendering. Install one of:
- **tectonic** (recommended): `winget install TectonicTypesetting.Tectonic`
- **texlive** with `latexmk`

### Interactive control (recommended)

For interactive use, start the local Web GUI:

```bash
lecture-distill gui --host 127.0.0.1 --port 8765
```

Or during development:

```bash
cargo run -- gui --host 127.0.0.1 --port 8765
```

Then open http://127.0.0.1:8765 in your browser. The GUI provides separate pages for each pipeline stage with forms, job status, and log output. JAAuthCookie and API keys are used in memory only and never saved to disk.

### Available commands

```bash
# Start the Web GUI (preferred for interactive control)
lecture-distill gui --host 127.0.0.1 --port 8765 --project-dir .lecture-distill/projects/default

# List videos in a Canvas course
lecture-distill canvas list-videos --course-id 12345 --cookie "your-ja-auth-cookie"

# Fetch subtitles for all videos in a course
lecture-distill canvas fetch-subtitles --course-id 12345 --cookie "your-ja-auth-cookie" --out data/transcripts

# Fetch subtitles for a specific video
lecture-distill canvas fetch-subtitles --course-id 12345 --cookie "your-ja-auth-cookie" --out data/transcripts --video-id abc123

# Patch existing notes with transcript content
lecture-distill patch-notes --notes notes.md --transcripts data/transcripts --out notes.patched.md --patches patches.json

# Distill patched notes into exam-focused summary
lecture-distill distill --notes notes.patched.md --out distilled.md

# Render distilled notes as a compact LaTeX cheat sheet PDF
lecture-distill render-cheatsheet --input distilled.md --template template.tex --out build/cheatsheet.pdf --max-pages 2

# Run the full pipeline end-to-end
lecture-distill run --course-id 12345 --cookie "your-ja-auth-cookie" --notes notes.md --out build/cheatsheet.pdf

# Show version
lecture-distill --version
```

### How to get a JAAuthCookie

1. Log in to https://oc.sjtu.edu.cn in your browser
2. Open Developer Tools -> Application -> Cookies
3. Copy the value of `JAAuthCookie`

**Never commit cookies to version control.** They are sensitive credentials.

### When the LLM is unavailable

All commands work without an LLM using deterministic fallbacks:
- `patch-notes`: Extracts key terms from transcripts
- `distill`: Uses heading/formula/bold-term extraction
- The output will be less nuanced but still functional

### Development

```bash
# Build
cargo build

# Run tests
cargo test

# Format check
cargo fmt --check

# CLI help
cargo run -- --help
cargo run -- gui --help
```

### Limitations (MVP)

- Only SJTU Canvas video flow is supported (not generic Canvas/Panopto)
- Videos are not downloaded; only subtitles/transcripts are fetched
- LaTeX compilation requires a local LaTeX installation

### Legacy Python (reference only)

The Python implementation at `lecture_distill/` is a reference implementation and is **not** the recommended usage path. To use it:

```bash
pip install -e .
lecture-distill --help
```
