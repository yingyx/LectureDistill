//! End-to-end integration tests for the lecture-distill pipeline.
//!
//! These tests exercise non-network parts of the pipeline:
//! - SRT parse -> write -> parse roundtrip
//! - Notes patch with deterministic mode doesn't modify source
//! - State save/load roundtrip with secret filtering
//! - Job registry create -> update -> list flow
//! - Distill deterministic produces expected structure, headings are shifted
//!
//! All tests use `tempfile` for file operations - no network, no Canvas.

use std::fs;
use tempfile::TempDir;

// Data types
use lecture_distill::artifacts::{
    KeepLevel, NoteArtifact, PatchArtifact, PatchEntry, RankedConcept, TranscriptArtifact,
    TranscriptSegment,
};

// Transcripts
use lecture_distill::transcripts;

// Notes
use lecture_distill::notes;

// Distill
use lecture_distill::distill;

// Web state
use lecture_distill::web::state::{is_forbidden, ProjectState, ProjectStateStore};

// Jobs
use lecture_distill::web::jobs::{JobRegistry, JobStatus};

// ===========================================================================
// SRT parse -> write -> parse roundtrip
// ===========================================================================

/// Multi-segment SRT roundtrip: str -> parse -> segments -> serialize -> parse again.
/// Timestamps should survive with sub-millisecond precision.
#[test]
fn test_srt_roundtrip_multi_segment() {
    let input = "\
1
00:00:01,000 --> 00:00:03,500
Hello world

2
00:00:05,000 --> 00:00:08,250
Second subtitle with special chars: & < > \" '

3
00:01:00,000 --> 00:01:02,000
Third line
";

    let segments_a = transcripts::parse_srt(input);
    // Re-serialise.
    let srt = transcripts::segments_to_srt(&segments_a);
    // Parse the output.
    let segments_b = transcripts::parse_srt(&srt);

    assert_eq!(segments_a.len(), segments_b.len());
    assert_eq!(segments_a.len(), 3);

    for (a, b) in segments_a.iter().zip(segments_b.iter()) {
        assert_eq!(a.index, b.index);
        assert!((a.start_time - b.start_time).abs() < 1e-6);
        assert!((a.end_time - b.end_time).abs() < 1e-6);
        assert_eq!(a.text, b.text);
    }
}

/// Roundtrip with VTT-style period millisecond separator.
#[test]
fn test_srt_roundtrip_vtt_style() {
    let input = "\
1
00:00:01.500 --> 00:00:03.750
VTT-style timestamps
";
    let segments = transcripts::parse_srt(input);
    assert_eq!(segments.len(), 1);
    assert!((segments[0].start_time - 1.5).abs() < 1e-6);
    assert!((segments[0].end_time - 3.75).abs() < 1e-6);
}

/// Roundtrip with empty input.
#[test]
fn test_srt_roundtrip_empty() {
    let segments = transcripts::parse_srt("");
    assert!(segments.is_empty());

    let srt = transcripts::segments_to_srt(&[]);
    assert!(srt.is_empty());
}

/// Format a timestamp and parse it back - should be lossless.
#[test]
fn test_format_then_parse_roundtrip() {
    for seconds in &[0.0, 1.5, 61.0, 3661.125, 86399.999] {
        let formatted = transcripts::format_timestamp(*seconds);
        let parsed = transcripts::parse_single_timestamp(&formatted).unwrap();
        assert!(
            (parsed - seconds).abs() < 1e-6,
            "roundtrip failed for {} s: formatted '{}' parsed to {}",
            seconds,
            formatted,
            parsed
        );
    }
}

/// transcript_to_srt helper produces valid SRT.
#[test]
fn test_transcript_to_srt_output() {
    let artifact = TranscriptArtifact {
        video_id: "vid1".into(),
        video_title: "Test".into(),
        course_id: "c1".into(),
        language: "zh".into(),
        segments: vec![
            TranscriptSegment {
                index: 1,
                start_time: 0.0,
                end_time: 2.0,
                text: "Hello".into(),
            },
            TranscriptSegment {
                index: 2,
                start_time: 3.0,
                end_time: 5.0,
                text: "World".into(),
            },
        ],
        fetched_at: "2025-01-01T00:00:00Z".into(),
        recorded_at: None,
        source_url: None,
    };

    let srt = transcripts::transcript_to_srt(&artifact);
    let reparsed = transcripts::parse_srt(&srt);

    assert_eq!(reparsed.len(), 2);
    assert_eq!(reparsed[0].text, "Hello");
    assert_eq!(reparsed[1].text, "World");
}

// ===========================================================================
// Notes patch: deterministic mode does NOT modify source file
// ===========================================================================

/// When the transcripts directory is empty, the source file must be unchanged
/// and the output files contain the original content plus conflict info.
#[test]
fn test_patch_notes_empty_transcripts_preserves_source() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = TempDir::new().unwrap();

    let notes_path = dir.path().join("source.md");
    let output_notes = dir.path().join("patched.md");
    let output_patches = dir.path().join("patches.json");
    let transcripts_dir = dir.path().join("tx");

    let original = "# Lecture 1\n\nContent here.\n";
    fs::write(&notes_path, original).unwrap();
    fs::create_dir(&transcripts_dir).unwrap();

    // Load the note directly to verify source state.
    let note_before = fs::read_to_string(&notes_path).unwrap();
    assert_eq!(note_before, original);

    let _result = rt.block_on(notes::patch_notes(
        notes_path.to_str().unwrap(),
        transcripts_dir.to_str().unwrap(),
        output_notes.to_str().unwrap(),
        output_patches.to_str().unwrap(),
    ));

    // patch_notes writes to output_notes and output_patches, never to
    // notes_path.  The source file must remain untouched.
    let note_after = fs::read_to_string(&notes_path).unwrap();
    assert_eq!(note_after, original, "source file was modified");
}

/// Deterministic patch extracts terms from transcripts but does not alter
/// the original notes content - it appends a "Transcript Additions" section.
#[test]
fn test_deterministic_patch_does_not_mutate_source() {
    let dir = TempDir::new().unwrap();

    let notes_path = dir.path().join("notes.md");
    let transcripts_dir = dir.path().join("tx");

    let note_content = "# Topics\n\nWe discussed **existing_concept** in class.\n";
    fs::write(&notes_path, note_content).unwrap();
    fs::create_dir(&transcripts_dir).unwrap();

    // Create a transcript with a term NOT already in the notes.
    let transcript = TranscriptArtifact {
        video_id: "v1".into(),
        video_title: "Lecture 1".into(),
        course_id: "c1".into(),
        language: "en".into(),
        segments: vec![TranscriptSegment {
            index: 1,
            start_time: 0.0,
            end_time: 5.0,
            text: r#"We introduced the "new_concept" in this lecture."#.into(),
        }],
        fetched_at: "2025-01-01T00:00:00Z".into(),
        recorded_at: None,
        source_url: None,
    };
    fs::write(
        transcripts_dir.join("v1.json"),
        serde_json::to_string(&transcript).unwrap(),
    )
    .unwrap();

    // Now test the deterministic patch function directly (not the full async
    // patch_notes, which would try the LLM path).
    let note = NoteArtifact {
        path: notes_path.to_str().unwrap().into(),
        content: note_content.into(),
        headings: vec!["Topics".into()],
    };

    let patch_result = notes::deterministic_patch(&note, &[transcript]);

    // The deterministic patcher should have found "new_concept".
    assert!(
        patch_result
            .patches
            .iter()
            .any(|p| p.new_text == "new_concept"),
        "expected 'new_concept' in patches, got {:?}",
        patch_result
            .patches
            .iter()
            .map(|p| &p.new_text)
            .collect::<Vec<_>>()
    );

    // "existing_concept" is already in the note, so it should NOT be
    // in patches (it's a bold term `**existing_concept**` in the note).
    let existing_patches: Vec<_> = patch_result
        .patches
        .iter()
        .filter(|p| p.new_text == "existing_concept")
        .collect();
    assert!(
        existing_patches.is_empty(),
        "'existing_concept' should be filtered (already in notes), found {}",
        existing_patches.len()
    );

    // The source file on disk must remain untouched.
    let on_disk = fs::read_to_string(&notes_path).unwrap();
    assert_eq!(on_disk, note_content);
}

// ===========================================================================
// State save/load roundtrip with secret filtering
// ===========================================================================

#[test]
fn test_state_forbidden_key_filtering() {
    assert!(is_forbidden("cookie"));
    assert!(is_forbidden("JAAuthCookie"));
    assert!(is_forbidden("api_key"));
    assert!(is_forbidden("api-key"));
    assert!(is_forbidden("OPENAI_API_KEY"));
    assert!(is_forbidden("PASSWORD"));
    assert!(is_forbidden("token"));
    assert!(is_forbidden("secret"));

    assert!(!is_forbidden("course_id"));
    assert!(!is_forbidden("notes_path"));
    assert!(!is_forbidden("transcripts_dir"));
    assert!(!is_forbidden("max_pages"));
    assert!(!is_forbidden("video_id"));
}

#[test]
fn test_state_cookie_never_serialized() {
    let mut state = ProjectState::default();
    state.course_id = "12345".into();
    state.cookie = "super-secret-session-cookie".into();

    let json = state.to_config_value();
    let json_str = json.to_string();

    assert!(!json_str.contains("cookie"));
    assert!(!json_str.contains("super-secret"));
    assert!(json_str.contains("12345"));
}

#[test]
fn test_state_cookie_not_deserialized_from_disk() {
    // Simulate a config file that somehow contains a cookie key.
    let data = serde_json::json!({
        "course_id": "67890",
        "cookie": "leaked-value"
    });
    let state = ProjectState::from_config_value(&data);
    assert_eq!(state.course_id, "67890");
    // Cookie field is #[serde(skip)] and not in ALLOWED_CONFIG_KEYS,
    // so it must remain empty.
    assert!(state.cookie.is_empty());
}

#[test]
fn test_state_store_full_roundtrip() {
    let dir = TempDir::new().unwrap();
    let store = ProjectStateStore::new(dir.path().to_str().unwrap());

    // Initial load returns defaults.
    let initial = store.load();
    assert!(initial.course_id.is_empty());
    assert_eq!(initial.max_pages, 2);

    // Update some fields.
    let fields = serde_json::json!({
        "course_id": "CS101",
        "transcripts_dir": "data/tx",
        "max_pages": 4,
        "notes_path": "my_notes.md"
    });
    let updated = store.update_and_save(&fields).unwrap();
    assert_eq!(updated.course_id, "CS101");
    assert_eq!(updated.transcripts_dir, "data/tx");
    assert_eq!(updated.max_pages, 4);
    assert_eq!(updated.notes_path, "my_notes.md");

    // Reload from disk and verify persistence.
    let reloaded = store.load();
    assert_eq!(reloaded.course_id, "CS101");
    assert_eq!(reloaded.transcripts_dir, "data/tx");
    assert_eq!(reloaded.max_pages, 4);
    assert_eq!(reloaded.notes_path, "my_notes.md");

    // Defaults for untouched fields should remain.
    assert_eq!(reloaded.output_notes, "artifacts/notes/notes.patched.md");
    assert_eq!(reloaded.output_pdf, "artifacts/outputs/cheatsheet.pdf");
}

#[test]
fn test_state_store_secret_not_written_to_disk() {
    let dir = TempDir::new().unwrap();
    let store = ProjectStateStore::new(dir.path().to_str().unwrap());

    // Try to save a forbidden key - it should be filtered.
    let fields = serde_json::json!({
        "course_id": "CS202",
        "cookie": "abc123secret",
        "api_key": "test-key-should-not-persist"
    });
    let _ = store.update_and_save(&fields).unwrap();

    // Read the raw config file from disk.
    let config_path = dir.path().join("config.json");
    let raw = fs::read_to_string(&config_path).unwrap();

    assert!(raw.contains("CS202"));
    assert!(!raw.contains("abc123secret"));
    assert!(!raw.contains("test-key-should-not-persist"));

    // Loading should only pick up course_id.
    let loaded = store.load();
    assert_eq!(loaded.course_id, "CS202");
    assert!(loaded.cookie.is_empty());
}

#[test]
fn test_state_store_defaults_when_config_missing() {
    let dir = TempDir::new().unwrap();
    let store = ProjectStateStore::new(dir.path().to_str().unwrap());

    // No config file exists yet.
    let state = store.load();
    assert!(state.course_id.is_empty());
    assert_eq!(state.max_pages, 2);
}

#[test]
fn test_state_store_corrupted_config_returns_defaults() {
    let dir = TempDir::new().unwrap();

    // Write garbage JSON to the config file.
    let config_path = dir.path().join("config.json");
    fs::write(&config_path, "not valid json {{{{{").unwrap();

    let store = ProjectStateStore::new(dir.path().to_str().unwrap());
    let state = store.load();

    // Should not panic, should return defaults.
    assert!(state.course_id.is_empty());
    assert_eq!(state.max_pages, 2);
}

// ===========================================================================
// Job registry: create -> update -> list flow
// ===========================================================================

#[test]
fn test_job_registry_create_get_update_list() {
    let registry = JobRegistry::new(10);

    // Create a job.
    let job = registry.create("test-pipeline");
    assert_eq!(job.name, "test-pipeline");
    assert_eq!(job.status, JobStatus::Pending);
    assert!(job.errors.is_empty());
    assert!(job.logs.is_empty());
    assert!(job.result.is_none());
    assert!(job.finished_at.is_none());

    let job_id = job.job_id.clone();

    // Retrieve it.
    let fetched = registry.get(&job_id).unwrap();
    assert_eq!(fetched.job_id, job_id);
    assert_eq!(fetched.name, "test-pipeline");

    // Update with log.
    registry.update(&job_id, None, Some("Step 1 complete"), None, None, None);
    let updated = registry.get(&job_id).unwrap();
    assert_eq!(updated.logs, vec!["Step 1 complete"]);

    // Mark as succeeded with an artifact and result.
    let result = serde_json::json!({"pages": 2, "size": 12345});
    registry.update(
        &job_id,
        Some(JobStatus::Succeeded),
        Some("Done"),
        None,
        Some("output.pdf"),
        Some(result.clone()),
    );
    let final_job = registry.get(&job_id).unwrap();
    assert_eq!(final_job.status, JobStatus::Succeeded);
    assert_eq!(final_job.logs.len(), 2);
    assert_eq!(final_job.logs[1], "Done");
    assert_eq!(final_job.artifact_paths, vec!["output.pdf"]);
    assert_eq!(final_job.result, Some(result));
    assert!(final_job.finished_at.is_some());
}

#[test]
fn test_job_registry_get_missing_returns_none() {
    let registry = JobRegistry::new(10);
    assert!(registry.get("nonexistent-id").is_none());
}

#[test]
fn test_job_registry_list_sorted_newest_first() {
    let registry = JobRegistry::new(10);

    let a = registry.create("job-a");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let b = registry.create("job-b");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let c = registry.create("job-c");

    let list = registry.list_jobs(10);
    assert_eq!(list.len(), 3);

    // Newest first.
    assert_eq!(list[0].job_id, c.job_id);
    assert_eq!(list[1].job_id, b.job_id);
    assert_eq!(list[2].job_id, a.job_id);
}

#[test]
fn test_job_registry_list_respects_limit() {
    let registry = JobRegistry::new(20);
    for i in 0..5 {
        registry.create(&format!("job-{}", i));
    }
    let list = registry.list_jobs(2);
    assert_eq!(list.len(), 2);
}

#[test]
fn test_job_registry_evicts_when_over_capacity() {
    let registry = JobRegistry::new(3);
    let first = registry.create("first");

    // Brief sleep to ensure different timestamps.
    for i in 1..6 {
        std::thread::sleep(std::time::Duration::from_millis(2));
        registry.create(&format!("job-{}", i));
    }

    let list = registry.list_jobs(10);
    assert!(list.len() <= 3, "should evict, got {} jobs", list.len());

    // The very first job should have been evicted.
    assert!(registry.get(&first.job_id).is_none());
}

#[test]
fn test_job_registry_update_missing_returns_none() {
    let registry = JobRegistry::new(10);
    let result = registry.update(
        "nonexistent",
        Some(JobStatus::Succeeded),
        None,
        None,
        None,
        None,
    );
    assert!(result.is_none());
}

#[test]
fn test_job_status_display() {
    assert_eq!(JobStatus::Pending.to_string(), "pending");
    assert_eq!(JobStatus::Running.to_string(), "running");
    assert_eq!(JobStatus::Succeeded.to_string(), "succeeded");
    assert_eq!(JobStatus::Failed.to_string(), "failed");
}

// ===========================================================================
// Distill deterministic produces expected structure
// ===========================================================================

#[test]
fn test_deterministic_distill_produces_expected_sections() {
    let content = "# Introduction\n\n## Key Principles\n\n### Details\n\nSome text here.\n";
    let concepts = vec![
        RankedConcept {
            name: "Principle A".into(),
            keep_level: KeepLevel::MustKeep,
            relevance_score: 0.95,
            source_headings: vec!["Key Principles".into()],
            timestamp_references: vec![],
            rationale: Some("Fundamental concept".into()),
        },
        RankedConcept {
            name: "Supporting B".into(),
            keep_level: KeepLevel::Compress,
            relevance_score: 0.7,
            source_headings: vec![],
            timestamp_references: vec![],
            rationale: None,
        },
    ];

    let output = distill::deterministic_distill(content, &concepts);

    // Title and mode note.
    assert!(output.contains("# Distilled Lecture Notes (Exam Review)"));
    assert!(output.contains("*Auto-generated by lecture-distill - deterministic mode*"));

    // Key concepts section.
    assert!(output.contains("## Key Concepts (Must Know)"));
    assert!(output.contains("**Principle A**"));
    assert!(output.contains("Fundamental concept"));

    // Supporting concepts section.
    assert!(output.contains("## Supporting Concepts (Understand)"));
    assert!(output.contains("**Supporting B**"));

    // Content summary with shifted headings.
    assert!(output.contains("## Content Summary"));
    assert!(output.contains("## Introduction"));
    assert!(output.contains("### Key Principles"));
    assert!(output.contains("#### Details"));
}

#[test]
fn test_deterministic_distill_heading_shift() {
    let content = "# H1\n\n## H2\n\n### H3\n\n#### H4\n\n##### H5\n\n###### H6\n";
    let output = distill::deterministic_distill(content, &[]);

    // Levels 1-3 are shifted down by one.
    assert!(output.contains("## H1\n"));
    assert!(output.contains("### H2\n"));
    assert!(output.contains("#### H3\n"));

    // Level 4+ headings should NOT appear (only 1-3 are captured).
    assert!(!output.contains("H4"));
    assert!(!output.contains("H5"));
    assert!(!output.contains("H6"));
}

#[test]
fn test_deterministic_distill_empty_concepts() {
    let content = "# Lecture\n\nSome text.\n";
    let output = distill::deterministic_distill(content, &[]);

    assert!(output.contains("## Key Concepts (Must Know)"));
    assert!(output.contains("*No must-know concepts identified.*"));
    assert!(output.contains("## Supporting Concepts (Understand)"));
    assert!(output.contains("*No supporting concepts identified.*"));
    assert!(output.contains("## Content Summary"));
}

#[test]
fn test_deterministic_distill_no_headings_in_source() {
    let content = "Just plain text with no markdown headings.\n";
    let output = distill::deterministic_distill(content, &[]);

    // Content summary should note the absence of headings.
    assert!(output.contains("*No headings found in source content.*"));
}

/// Deterministic distill should be predictable: same input -> same output.
#[test]
fn test_deterministic_distill_is_deterministic() {
    let content = "# Topic\n## Subtopic\n\ntext\n";
    let concepts = vec![RankedConcept {
        name: "Key".into(),
        keep_level: KeepLevel::MustKeep,
        relevance_score: 0.9,
        source_headings: vec![],
        timestamp_references: vec![],
        rationale: Some("reason".into()),
    }];

    let a = distill::deterministic_distill(content, &concepts);
    let b = distill::deterministic_distill(content, &concepts);
    assert_eq!(a, b);
}

// ===========================================================================
// apply_patches output structure
// ===========================================================================

#[test]
fn test_apply_patches_includes_original_content_and_sections() {
    let original = "# Notes\n\nOriginal text here.\n";
    let pa = PatchArtifact {
        source_notes_path: "notes.md".into(),
        source_transcripts: vec!["c1/v1".into()],
        patches: vec![PatchEntry {
            location: "Glossary".into(),
            original_text: None,
            new_text: "new term".into(),
            source_video_id: Some("v1".into()),
            source_timestamp: Some(42.0),
            keep_level: KeepLevel::Compress,
        }],
        conflicts: vec!["Manual review needed".into()],
        patched_at: "2025-01-01T00:00:00Z".into(),
    };

    let result = notes::apply_patches(original, &pa);

    // Original content is preserved (preceded by a comment line).
    assert!(result.contains("<!-- Patched by lecture-distill -->"));
    assert!(result.contains("Original text here."));

    // Transcript Additions section groups patches.
    assert!(result.contains("## Transcript Additions"));
    assert!(result.contains("### Glossary"));
    assert!(result.contains("new term"));
    assert!(result.contains("from v1 at 42.0s"));

    // Conflicts section.
    assert!(result.contains("## Conflicts / Needs Review"));
    assert!(result.contains("Manual review needed"));
}

#[test]
fn test_apply_patches_no_conflicts_section_when_empty() {
    let original = "# Notes\n\nContent.\n";
    let pa = PatchArtifact {
        source_notes_path: "notes.md".into(),
        source_transcripts: vec![],
        patches: vec![],
        conflicts: vec![],
        patched_at: "2025-01-01T00:00:00Z".into(),
    };

    let result = notes::apply_patches(original, &pa);

    assert!(result.contains("<!-- Patched by lecture-distill -->"));
    assert!(result.contains("Content."));
    // No conflicts section when empty.
    assert!(!result.contains("## Conflicts / Needs Review"));
    // No transcript additions when patches empty.
    assert!(!result.contains("## Transcript Additions"));
}

/// Multiple patches under the same location should be grouped under a
/// single ### heading.
#[test]
fn test_apply_patches_groups_by_location() {
    let original = "# Notes\n\nContent.\n";
    let pa = PatchArtifact {
        source_notes_path: "notes.md".into(),
        source_transcripts: vec![],
        patches: vec![
            PatchEntry {
                location: "Section A".into(),
                original_text: None,
                new_text: "term1".into(),
                source_video_id: None,
                source_timestamp: None,
                keep_level: KeepLevel::Compress,
            },
            PatchEntry {
                location: "Section A".into(),
                original_text: None,
                new_text: "term2".into(),
                source_video_id: None,
                source_timestamp: None,
                keep_level: KeepLevel::Compress,
            },
            PatchEntry {
                location: "Section B".into(),
                original_text: None,
                new_text: "term3".into(),
                source_video_id: None,
                source_timestamp: None,
                keep_level: KeepLevel::Compress,
            },
        ],
        conflicts: vec![],
        patched_at: "2025-01-01T00:00:00Z".into(),
    };

    let result = notes::apply_patches(original, &pa);

    // Section A heading should appear exactly once.
    let count = result.matches("### Section A").count();
    assert_eq!(count, 1, "Section A heading should appear exactly once");

    // Both terms should be under it.
    assert!(result.contains("term1"));
    assert!(result.contains("term2"));
    assert!(result.contains("term3"));
}

// ===========================================================================
// extract_headings
// ===========================================================================

#[test]
fn test_extract_headings_all_levels() {
    let content = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6\nSome text.\n";
    let headings = notes::extract_headings(content);

    assert_eq!(headings.len(), 6);
    assert_eq!(headings, vec!["H1", "H2", "H3", "H4", "H5", "H6"]);
}

#[test]
fn test_extract_headings_excludes_body_hashes() {
    let content = "## Real heading\nThis is not a ## heading because inline.\n### Another\n";
    let headings = notes::extract_headings(content);
    assert_eq!(headings, vec!["Real heading", "Another"]);
}

#[test]
fn test_extract_headings_empty_content() {
    let headings = notes::extract_headings("");
    assert!(headings.is_empty());
}

// ===========================================================================
// load_notes (uses tempfile)
// ===========================================================================

#[test]
fn test_load_notes_reads_file() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("lecture.md");
    fs::write(&file, "# Title\n\nBody.\n").unwrap();

    let note = notes::load_notes(file.to_str().unwrap()).unwrap();
    assert_eq!(note.content, "# Title\n\nBody.\n");
    assert_eq!(note.headings, vec!["Title"]);
    assert_eq!(note.path, file.to_str().unwrap());
}

#[test]
fn test_load_notes_file_not_found() {
    assert!(notes::load_notes("/definitely/does/not/exist.md").is_err());
}

// ===========================================================================
// Write outputs creates parent directories
// ===========================================================================

#[test]
fn test_write_outputs_creates_nested_dirs() {
    let dir = TempDir::new().unwrap();
    let nested_notes = dir.path().join("deep").join("sub").join("out.md");
    let nested_patches = dir.path().join("deep").join("sub").join("out.json");

    let pa = PatchArtifact::new("source.md".into());

    notes::write_outputs(
        "content",
        &pa,
        nested_notes.to_str().unwrap(),
        nested_patches.to_str().unwrap(),
    )
    .unwrap();

    assert!(nested_notes.exists());
    assert!(nested_patches.exists());
    assert_eq!(fs::read_to_string(&nested_notes).unwrap(), "content");
}
