//! Canvas handlers: POST /api/canvas/list-videos, POST /api/canvas/fetch-subtitles.

use axum::extract::State;
use axum::Json;

use crate::pipeline::{PipelineResult, PipelineRunner};
use crate::web::app::AppState;
use crate::web::jobs::JobStatus;

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/canvas/list-videos`
pub(crate) async fn api_canvas_list_videos(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let course_id = body
        .get("course_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cookie_input = body
        .get("cookie")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let saved_secrets = state.secrets.load();
    let cookie = if cookie_input.trim().is_empty() {
        saved_secrets.canvas_auth_cookie().unwrap_or_default()
    } else {
        cookie_input
    };

    if course_id.is_empty() || cookie.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "errors": ["Course ID and Cookie are required. Save a Canvas/jAccount cookie in Settings or paste one here."]
        }));
    }

    // Save non-secret state.
    let fields = serde_json::json!({"course_id": course_id.as_str()});
    let _ = state.store.update_and_save(&fields);

    let registry = state.registry.clone();
    let registry_clone = registry.clone();
    let cid = course_id;
    let ck = cookie;

    let job = registry.run_in_background("list-videos", move |job| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut client = crate::canvas_sjtu::CanvasSJTUVideoClient::new(cid.clone(), ck);
            match client.list_videos().await {
                Ok(videos) => {
                    let video_list: Vec<serde_json::Value> = videos
                        .iter()
                        .map(|v| {
                            serde_json::json!({
                                "video_id": v.video_id,
                                "title": v.title,
                                "duration": v.duration,
                                "course_begin_time": v.course_begin_time,
                                "course_end_time": v.course_end_time,
                                "teacher": v.teacher,
                                "classroom": v.classroom,
                            })
                        })
                        .collect();
                    let result = serde_json::json!({
                        "videos": video_list,
                        "count": video_list.len(),
                    });
                    registry_clone.update(
                        &job.job_id,
                        Some(JobStatus::Succeeded),
                        Some(&format!("Found {} video(s)", video_list.len())),
                        None,
                        None,
                        Some(result),
                    );
                }
                Err(e) => {
                    registry_clone.update(
                        &job.job_id,
                        Some(JobStatus::Failed),
                        None,
                        Some(&e.to_string()),
                        None,
                        None,
                    );
                }
            }
        });
    });

    Json(serde_json::json!({
        "job_id": job.job_id,
        "status": "running"
    }))
}

/// `POST /api/canvas/fetch-subtitles`
///
/// Accepts `scope` (course|day|selected), optional `date`, optional
/// `video_ids` array.
pub(crate) async fn api_canvas_fetch_subtitles(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let course_id = body
        .get("course_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cookie_input = body
        .get("cookie")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let saved_secrets = state.secrets.load();
    let cookie = if cookie_input.trim().is_empty() {
        saved_secrets.canvas_auth_cookie().unwrap_or_default()
    } else {
        cookie_input
    };
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("course")
        .to_string();
    let date = body
        .get("date")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let video_ids: Vec<String> = body
        .get("video_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let transcripts_dir = body
        .get("transcripts_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("artifacts/transcripts")
        .to_string();

    if course_id.is_empty() || cookie.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "errors": ["Course ID and Cookie are required. Save a Canvas/jAccount cookie in Settings or paste one here."]
        }));
    }

    let fields = serde_json::json!({
        "course_id": course_id.as_str(),
        "transcripts_dir": transcripts_dir.as_str(),
    });
    let _ = state.store.update_and_save(&fields);

    let registry = state.registry.clone();
    let registry_clone = registry.clone();

    let job = registry.run_in_background("fetch-subtitles", move |job| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let runner = match PipelineRunner::new(".") {
                Ok(r) => r,
                Err(e) => {
                    registry_clone.update(
                        &job.job_id,
                        Some(JobStatus::Failed),
                        None,
                        Some(&format!("Failed to create pipeline runner: {}", e)),
                        None,
                        None,
                    );
                    return;
                }
            };

            let result = match scope.as_str() {
                "selected" if !video_ids.is_empty() => {
                    // Fetch selected videos one by one.
                    let mut all_ok = true;
                    let mut errors: Vec<String> = Vec::new();
                    let mut logs: Vec<String> = Vec::new();
                    let mut paths: Vec<String> = Vec::new();

                    for vid in &video_ids {
                        match runner
                            .fetch_transcripts(&course_id, &cookie, Some(vid), &transcripts_dir)
                            .await
                        {
                            res if res.ok() => {
                                paths.extend(res.artifact_paths);
                                logs.extend(res.logs);
                            }
                            res => {
                                all_ok = false;
                                errors.extend(res.errors);
                                logs.extend(res.logs);
                            }
                        }
                    }

                    PipelineResult {
                        status: if all_ok {
                            "succeeded".to_string()
                        } else {
                            "failed".to_string()
                        },
                        artifact_paths: paths,
                        errors,
                        logs,
                        space_utilization: None,
                    }
                }
                "day" if date.is_some() => {
                    // Fetch all, then filter by date.
                    let result = runner
                        .fetch_transcripts(&course_id, &cookie, None, &transcripts_dir)
                        .await;
                    result
                }
                _ => {
                    // "course" -- fetch all.
                    runner
                        .fetch_transcripts(&course_id, &cookie, None, &transcripts_dir)
                        .await
                }
            };

            let result_json = serde_json::to_value(&result).unwrap_or_default();
            registry_clone.update(
                &job.job_id,
                Some(if result.ok() {
                    JobStatus::Succeeded
                } else {
                    JobStatus::Failed
                }),
                None,
                None,
                None,
                Some(result_json),
            );
        });
    });

    Json(serde_json::json!({
        "job_id": job.job_id,
        "status": "running"
    }))
}
