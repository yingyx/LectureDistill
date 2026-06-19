//! Course handlers: GET /api/canvas/courses, GET /api/canvas/course-dates.

use axum::{
    extract::{Query, State},
    response::Json,
};
use serde::Deserialize;
use std::collections::HashMap;

use crate::web::app::AppState;

// ---------------------------------------------------------------------------
// Canvas LMS courses API
// ---------------------------------------------------------------------------

/// `GET /api/canvas/courses` -- list available Canvas courses.
///
/// Uses the saved Canvas token from secrets, or a token provided via
/// `Authorization` header or `?token=` query parameter.
///
/// Calls the official Canvas LMS REST API:
/// `GET https://oc.sjtu.edu.cn/api/v1/courses?include[]=term&include[]=teachers&state[]=available&per_page=100`
#[derive(Debug, Deserialize)]
pub(crate) struct CanvasCoursesQuery {
    #[serde(default)]
    pub token: String,
}

pub(crate) async fn api_canvas_courses(
    State(state): State<AppState>,
    Query(query): Query<CanvasCoursesQuery>,
) -> Json<serde_json::Value> {
    // Resolve token: query param > secrets. We do NOT read Authorization header
    // here to keep the implementation simple; the query param or saved token
    // covers the primary use case.
    let saved_secrets = state.secrets.load();
    let token = if query.token.trim().is_empty() {
        if saved_secrets.canvas_token.is_empty() {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "No Canvas API token available. Save a Canvas token in Settings or pass ?token=... query parameter.",
                "courses": [],
            }));
        }
        saved_secrets.canvas_token.clone()
    } else {
        query.token.trim().to_string()
    };

    let url = "https://oc.sjtu.edu.cn/api/v1/courses?include[]=term&include[]=teachers&state[]=available&per_page=100";

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to create HTTP client: {}", e),
                "courses": [],
            }));
        }
    };

    let resp = match client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to connect to Canvas API: {}", e),
                "courses": [],
            }));
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        return Json(serde_json::json!({
            "status": "failed",
            "error": format!("Canvas API returned HTTP {}: {}", status.as_u16(), body_text),
            "courses": [],
        }));
    }

    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to parse Canvas API response: {}", e),
                "courses": [],
            }));
        }
    };

    // Parse courses into a simple list.
    let courses: Vec<serde_json::Value> = json
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|course| {
                    let term_name = course
                        .get("term")
                        .and_then(|t| t.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let teachers: Vec<&str> = course
                        .get("teachers")
                        .and_then(|t| t.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|t| {
                                    t.get("display_name").and_then(|v| v.as_str())
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    serde_json::json!({
                        "id": course.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
                        "name": course.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        "course_code": course.get("course_code").and_then(|v| v.as_str()).unwrap_or(""),
                        "start_at": course.get("start_at").and_then(|v| v.as_str()),
                        "end_at": course.get("end_at").and_then(|v| v.as_str()),
                        "workflow_state": course.get("workflow_state").and_then(|v| v.as_str()).unwrap_or(""),
                        "enrollment_term_id": course.get("enrollment_term_id").and_then(|v| v.as_u64()),
                        "term_name": term_name,
                        "teachers": teachers,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Json(serde_json::json!({
        "status": "succeeded",
        "courses": courses,
        "count": courses.len(),
    }))
}

// ---------------------------------------------------------------------------
// Canvas course dates API
// ---------------------------------------------------------------------------

/// `GET /api/canvas/course-dates?course_id=...`
///
/// Uses the saved Canvas/JAccount cookie from Settings to list all videos for a
/// course, then groups them by date (derived from `course_begin_time`).
/// Returns a sorted date list suitable for a dropdown picker.
#[derive(Debug, Deserialize)]
pub(crate) struct CourseDatesQuery {
    pub course_id: String,
}

pub(crate) async fn api_canvas_course_dates(
    State(state): State<AppState>,
    Query(query): Query<CourseDatesQuery>,
) -> Json<serde_json::Value> {
    if query.course_id.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "course_id query parameter is required.",
        }));
    }

    let saved_secrets = state.secrets.load();
    let cookie = match saved_secrets.canvas_auth_cookie() {
        Some(c) => c,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "No Canvas video credential saved. Go to Settings and save Canvas credentials first.",
            }));
        }
    };

    let mut client =
        crate::canvas_sjtu::CanvasSJTUVideoClient::new(query.course_id.clone(), cookie);

    let videos = match client.list_videos().await {
        Ok(v) => v,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to list videos: {}", e),
            }));
        }
    };

    // Group videos by date and build the date list.
    let dates = group_videos_by_date(&videos);

    Json(serde_json::json!({
        "status": "succeeded",
        "course_id": query.course_id,
        "total_videos": videos.len(),
        "dates": dates,
    }))
}

// ---------------------------------------------------------------------------
// Shared helpers for grouping videos by date
// ---------------------------------------------------------------------------

/// Group video infos by date, returning a sorted list of date entries.
///
/// Each entry contains the date, video count, and the first/last video title.
/// Dates are extracted from `course_begin_time` (first 10 chars = YYYY-MM-DD),
/// with a fallback to title-based date extraction.
pub(crate) fn group_videos_by_date(
    videos: &[crate::canvas_sjtu::CanvasVideoInfo],
) -> Vec<serde_json::Value> {
    let mut date_map: HashMap<String, Vec<&crate::canvas_sjtu::CanvasVideoInfo>> = HashMap::new();

    for v in videos {
        let date = extract_date_from_video(v);
        date_map.entry(date).or_default().push(v);
    }

    let mut dates: Vec<serde_json::Value> = date_map
        .into_iter()
        .map(|(date, vids)| {
            let mut sorted_vids = vids.clone();
            sorted_vids.sort_by(|a, b| {
                a.course_begin_time
                    .cmp(&b.course_begin_time)
                    .then_with(|| a.title.cmp(&b.title))
            });
            let first_title = sorted_vids.first().map(|v| v.title.clone());
            let last_title = sorted_vids.last().map(|v| v.title.clone());
            serde_json::json!({
                "date": date,
                "video_count": sorted_vids.len(),
                "first_title": first_title,
                "last_title": last_title,
            })
        })
        .collect();

    // Sort newest first.
    dates.sort_by(|a, b| {
        b["date"]
            .as_str()
            .unwrap_or("")
            .cmp(a["date"].as_str().unwrap_or(""))
    });

    dates
}

/// Extract a date string (YYYY-MM-DD) from a video info.
///
/// Uses `course_begin_time` first, falling back to date extraction from the
/// video title if the begin time is not parseable.
pub(crate) fn extract_date_from_video(v: &crate::canvas_sjtu::CanvasVideoInfo) -> String {
    // course_begin_time is typically "2023-09-18 08:00:00" or similar.
    if v.course_begin_time.len() >= 10 {
        let date_part = &v.course_begin_time[..10];
        // Validate it looks like YYYY-MM-DD.
        if date_part.len() == 10
            && date_part.chars().nth(4) == Some('-')
            && date_part.chars().nth(7) == Some('-')
            && date_part[..4].chars().all(|c| c.is_ascii_digit())
        {
            return date_part.to_string();
        }
    }

    // Fallback: try to extract a date from the video title.
    // Common patterns: "2023-09-18", "20230918", "09-18", etc.
    extract_date_from_title(&v.title)
}

/// Try to extract a YYYY-MM-DD date from a video title string.
fn extract_date_from_title(title: &str) -> String {
    use regex::Regex;
    // Try YYYY-MM-DD or YYYY/MM/DD.
    if let Ok(re) = Regex::new(r"(\d{4})[-/](\d{2})[-/](\d{2})") {
        if let Some(caps) = re.captures(title) {
            return format!("{}-{}-{}", &caps[1], &caps[2], &caps[3]);
        }
    }
    // Try YYYYMMDD.
    if let Ok(re) = Regex::new(r"(\d{4})(\d{2})(\d{2})") {
        if let Some(caps) = re.captures(title) {
            return format!("{}-{}-{}", &caps[1], &caps[2], &caps[3]);
        }
    }
    // Fallback: use a placeholder.
    "unknown-date".to_string()
}
