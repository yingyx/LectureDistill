//! CLI help smoke tests for lecture-distill.
//!
//! These tests invoke the compiled binary and check that `--help` and
//! `--version` produce the expected output.  They also verify that
//! subcommands without required arguments fail with a meaningful message.

use assert_cmd::Command;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Top-level help and version
// ---------------------------------------------------------------------------

#[test]
fn test_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("lecture-distill"))
        .stdout(predicate::str::contains("canvas"))
        .stdout(predicate::str::contains("patch-notes"))
        .stdout(predicate::str::contains("distill"))
        .stdout(predicate::str::contains("render-cheatsheet"))
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("gui"));
}

#[test]
fn test_version() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.arg("--version");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("lecture-distill"));
}

// ---------------------------------------------------------------------------
// Subcommand --help
// ---------------------------------------------------------------------------

#[test]
fn test_canvas_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["canvas", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("list-videos"))
        .stdout(predicate::str::contains("fetch-subtitles"));
}

#[test]
fn test_canvas_list_videos_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["canvas", "list-videos", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("course-id"))
        .stdout(predicate::str::contains("cookie"));
}

#[test]
fn test_canvas_fetch_subtitles_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["canvas", "fetch-subtitles", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("course-id"))
        .stdout(predicate::str::contains("cookie"))
        .stdout(predicate::str::contains("out"));
}

#[test]
fn test_patch_notes_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["patch-notes", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("notes"))
        .stdout(predicate::str::contains("transcripts"))
        .stdout(predicate::str::contains("out"))
        .stdout(predicate::str::contains("patches"));
}

#[test]
fn test_distill_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["distill", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("notes"))
        .stdout(predicate::str::contains("out"));
}

#[test]
fn test_render_cheatsheet_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["render-cheatsheet", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("input"))
        .stdout(predicate::str::contains("template"))
        .stdout(predicate::str::contains("out"))
        .stdout(predicate::str::contains("max-pages"));
}

#[test]
fn test_run_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["run", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("course-id"))
        .stdout(predicate::str::contains("cookie"))
        .stdout(predicate::str::contains("notes"))
        .stdout(predicate::str::contains("out"))
        .stdout(predicate::str::contains("max-pages"));
}

#[test]
fn test_gui_help() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["gui", "--help"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("host"))
        .stdout(predicate::str::contains("port"))
        .stdout(predicate::str::contains("project-dir"));
}

// ---------------------------------------------------------------------------
// Graceful failure when required args are missing
// ---------------------------------------------------------------------------

#[test]
fn test_canvas_no_args_fails() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.arg("canvas");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("Usage"))
        .stderr(predicate::str::contains("Commands"));
}

#[test]
fn test_canvas_list_videos_no_args_fails() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["canvas", "list-videos"]);
    // Without --course-id and --cookie, it should fail with a missing-arg
    // error since both are required.
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("course-id").or(predicate::str::contains("required")));
}

#[test]
fn test_patch_notes_no_args_fails() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["patch-notes"]);
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("notes").or(predicate::str::contains("required")));
}

#[test]
fn test_distill_no_args_fails() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["distill"]);
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("notes").or(predicate::str::contains("required")));
}

#[test]
fn test_render_cheatsheet_no_args_fails() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["render-cheatsheet"]);
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("input").or(predicate::str::contains("required")));
}

/// Full pipeline requires course-id, cookie, notes -- all missing should fail.
#[test]
fn test_run_no_args_fails() {
    let mut cmd = Command::cargo_bin("lecture-distill").unwrap();
    cmd.args(["run"]);
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("course-id").or(predicate::str::contains("required")));
}
