//! SJTU Canvas video subtitle connector.
//!
//! Handles LTI/OIDC authentication with the SJTU video platform,
//! lists course videos, and fetches subtitles/transcripts.
//!
//! The authentication flow is a 5-step LTI launch:
//! 1. GET the Canvas external tool page (sets OIDC cookies)
//! 2. Parse the first form -> POST to OIDC login_initiations
//! 3. Parse the second form -> POST to LTI3 auth (capture redirect)
//! 4. Extract `tokenId` from the `Location` header
//! 5. Exchange `tokenId` for an access token + canvas course ID

use anyhow::{Context, Result};
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::sync::Arc;

use crate::artifacts::{TranscriptArtifact, TranscriptSegment};
// `crate::transcripts::transcript_to_srt` is available for callers that need
// to convert a TranscriptArtifact to an SRT string after fetching subtitles.

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CANVAS_BASE: &str = "https://oc.sjtu.edu.cn";
const VIDEO_BASE: &str = "https://v.sjtu.edu.cn";
const EXTERNAL_TOOL_PATH: &str = "/courses/{course_id}/external_tools/8329";
const AUTH_URL: &str = "https://jaccount.sjtu.edu.cn";
const MY_SJTU_URL: &str = "https://my.sjtu.edu.cn/ui/appmyinfo";
const CANVAS_LOGIN_URL: &str = "https://oc.sjtu.edu.cn/login/openid_connect";
const OIDC_ACTION: &str = "https://v.sjtu.edu.cn/jy-application-canvas-sjtu/oidc/login_initiations";
const LTI3_AUTH_ACTION: &str = "https://v.sjtu.edu.cn/jy-application-canvas-sjtu/lti3/lti3Auth/ivs";

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Information about a single video on SJTU Canvas.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasVideoInfo {
    pub video_id: String,
    pub title: String,
    pub duration: u64, // seconds
    #[serde(default)]
    pub course_begin_time: String,
    #[serde(default)]
    pub course_end_time: String,
    #[serde(default)]
    pub teacher: String,
    #[serde(default)]
    pub classroom: String,
}

/// One computer/PPT screenshot slice from the SJTU video analysis endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasPptSlice {
    /// Screenshot timestamp in seconds from the beginning of the video.
    pub create_sec: f64,
    #[serde(default)]
    pub ppt_img_url: Option<String>,
    #[serde(default)]
    pub ocr_words: Vec<String>,
}

/// An authenticated session with the SJTU video platform.
#[derive(Debug, Clone)]
pub struct CanvasSession {
    course_id: String,
    token: String,
    canvas_course_id: String,
}

/// Client for interacting with SJTU Canvas video platform.
pub struct CanvasSJTUVideoClient {
    course_id: String,
    cookies: Arc<reqwest::cookie::Jar>,
    client: reqwest::Client,
    session: Option<CanvasSession>,
}

// ---------------------------------------------------------------------------
// Helper: form extraction
// ---------------------------------------------------------------------------

/// Extract form action URL and input name/value pairs from HTML.
///
/// Returns `(action_url, inputs_vec)` where each input entry is `(name, value)`.
/// Only `<input>` elements with a `name` attribute are included.
#[cfg(test)]
fn extract_form_action_and_inputs(html: &str) -> Result<(String, Vec<(String, String)>)> {
    let doc = Html::parse_document(html);
    let form_sel = Selector::parse("form").unwrap();
    let input_sel = Selector::parse("input").unwrap();

    let form = doc
        .select(&form_sel)
        .next()
        .context("No form element found in HTML")?;

    let action = form
        .value()
        .attr("action")
        .context("Form element has no action attribute")?
        .to_string();

    let mut inputs = Vec::new();
    for input in form.select(&input_sel) {
        if let Some(name) = input.value().attr("name") {
            let value = input.value().attr("value").unwrap_or("");
            inputs.push((name.to_string(), value.to_string()));
        }
    }

    Ok((action, inputs))
}

/// Extract input data from the form whose action exactly matches `action_url`.
///
/// SJTU-Canvas-Helper selects forms this way instead of taking the first form
/// on the page. That avoids accidentally submitting login/error-page forms.
fn extract_form_inputs_for_action(html: &str, action_url: &str) -> Result<Vec<(String, String)>> {
    let doc = Html::parse_document(html);
    let form_sel = Selector::parse("form").unwrap();
    let input_sel = Selector::parse("input").unwrap();

    let form = doc
        .select(&form_sel)
        .find(|form| form.value().attr("action") == Some(action_url))
        .with_context(|| format!("No form found with action {action_url}"))?;

    let mut inputs = Vec::new();
    for input in form.select(&input_sel) {
        if let Some(name) = input.value().attr("name") {
            if let Some(value) = input.value().attr("value") {
                inputs.push((name.to_string(), value.to_string()));
            }
        }
    }

    Ok(inputs)
}

fn value_to_string(value: &serde_json::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        Some(s.to_string())
    } else if let Some(n) = value.as_i64() {
        Some(n.to_string())
    } else if let Some(n) = value.as_u64() {
        Some(n.to_string())
    } else {
        value.as_f64().map(|n| n.to_string())
    }
}

// ---------------------------------------------------------------------------
// Helper: token-id extraction
// ---------------------------------------------------------------------------

/// Extract `tokenId` from a Location header URL.
///
/// Tries URL query-param extraction first, then falls back to regex for
/// path-embedded tokens (`tokenId=...`  or  `tokenId/...`).
fn extract_token_id(location: &str) -> Result<String> {
    // 1) URL query parameter.
    if let Ok(url) = url::Url::parse(location) {
        for (key, value) in url.query_pairs() {
            if key == "tokenId" {
                return Ok(value.into_owned());
            }
        }
    }

    // 2) Regex fallback - `tokenId=value`
    let re_eq = regex::Regex::new(r"tokenId=([^&\s/?#]+)").unwrap();
    if let Some(caps) = re_eq.captures(location) {
        return Ok(caps[1].to_string());
    }

    // 3) Regex fallback - `tokenId/value` (path segment)
    let re_path = regex::Regex::new(r"tokenId/([^/\s?#]+)").unwrap();
    if let Some(caps) = re_path.captures(location) {
        return Ok(caps[1].to_string());
    }

    anyhow::bail!("Could not extract tokenId from: {}", location)
}

// ---------------------------------------------------------------------------
// Helper: subtitle segment extraction
// ---------------------------------------------------------------------------

/// Convert SJTU subtitle JSON items to [`TranscriptSegment`]s.
///
/// Field names tried (in order):
/// - start time: `bg`, `startTime`, `start`
/// - end time:   `ed`, `endTime`, `end`
/// - text:       `res`, `oneSentence`
///
/// SJTU values for `bg`/`ed` are **milliseconds** - they are divided by 1000
/// to produce second values.  Items whose text is empty after trimming are
/// silently skipped.
fn segments_from_canvas_subtitles(items: &[serde_json::Value]) -> Vec<TranscriptSegment> {
    items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            // --- text ---
            let text = item
                .get("res")
                .or_else(|| item.get("oneSentence"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();

            if text.is_empty() {
                return None;
            }

            // --- start time (ms -> s) ---
            let start_ms = item
                .get("bg")
                .or_else(|| item.get("startTime"))
                .or_else(|| item.get("start"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);

            // --- end time (ms -> s) ---
            let end_ms = item
                .get("ed")
                .or_else(|| item.get("endTime"))
                .or_else(|| item.get("end"))
                .and_then(|v| v.as_f64())
                .unwrap_or(start_ms + 1000.0);

            Some(TranscriptSegment {
                index: i + 1,
                start_time: start_ms / 1000.0,
                end_time: end_ms / 1000.0,
                text,
            })
        })
        .collect()
}

/// Multi-strategy subtitle text extraction from an opaque JSON value.
///
/// Fallback order:
/// 1. The value itself if it is a string.
/// 2. Known top-level keys: `subtitles`, `captions`, `srtContent`,
///    `subtitleContent`, `content`, `text`.
/// 3. Nested list keys (`records`, `list`, `items`, `subtitles`) - if found,
///    join the segment texts with newlines.
/// 4. `serde_json::to_string_pretty` of the whole value.
#[cfg(test)]
fn extract_subtitle_text(data: &serde_json::Value) -> String {
    // 1) Direct string.
    if let Some(s) = data.as_str() {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }

    // 2) Known top-level string keys.
    for key in &[
        "subtitles",
        "captions",
        "srtContent",
        "subtitleContent",
        "content",
        "text",
    ] {
        if let Some(v) = data.get(key).and_then(|v| v.as_str()) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }

    // 3) Nested list keys - join segment texts.
    for key in &["records", "list", "items", "subtitles"] {
        if let Some(arr) = data.get(key).and_then(|v| v.as_array()) {
            let segments = segments_from_canvas_subtitles(arr);
            if !segments.is_empty() {
                return segments
                    .iter()
                    .map(|s| s.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        }
    }

    // 4) Final fallback.
    serde_json::to_string_pretty(data).unwrap_or_default()
}

/// Extract subtitle segments from the `translate/detail` API response.
///
/// Searches `beforeAssemblyList` and `afterAssemblyList` at the top level,
/// then inside `data` / `result` wrappers, then falls back to treating the
/// top-level value as an array.
fn extract_subtitle_segments(data: &serde_json::Value) -> Vec<TranscriptSegment> {
    // Direct array keys at top level.
    for key in &["beforeAssemblyList", "afterAssemblyList"] {
        if let Some(list) = data.get(key).and_then(|v| v.as_array()) {
            let segments = segments_from_canvas_subtitles(list);
            if !segments.is_empty() {
                return segments;
            }
        }
    }

    // Top-level is an array itself.
    if let Some(arr) = data.as_array() {
        let segments = segments_from_canvas_subtitles(arr);
        if !segments.is_empty() {
            return segments;
        }
    }

    // Nested under `data` or `result`.
    for wrapper in &["data", "result"] {
        if let Some(inner) = data.get(wrapper) {
            for list_key in &[
                "beforeAssemblyList",
                "afterAssemblyList",
                "subtitles",
                "list",
                "records",
            ] {
                if let Some(list) = inner.get(list_key).and_then(|v| v.as_array()) {
                    let segments = segments_from_canvas_subtitles(list);
                    if !segments.is_empty() {
                        return segments;
                    }
                }
            }
            // Also try if `inner` itself is an array.
            if let Some(arr) = inner.as_array() {
                let segments = segments_from_canvas_subtitles(arr);
                if !segments.is_empty() {
                    return segments;
                }
            }
        }
    }

    Vec::new()
}

// ---------------------------------------------------------------------------
// CanvasSJTUVideoClient
// ---------------------------------------------------------------------------

impl CanvasSJTUVideoClient {
    /// Create a new client.
    ///
    /// `ja_auth_cookie` is the value of the `JAAuthCookie` cookie obtained
    /// from a browser session on `oc.sjtu.edu.cn`.
    pub fn new(course_id: String, ja_auth_cookie: String) -> Self {
        let jar = Arc::new(reqwest::cookie::Jar::default());

        // SJTU-Canvas-Helper passes a full `JAAuthCookie=...` pair to the
        // jAccount/mySJTU/Canvas hosts before probing protected pages.
        let cookie_pair = if ja_auth_cookie.trim_start().starts_with("JAAuthCookie=") {
            ja_auth_cookie.trim().to_string()
        } else {
            format!("JAAuthCookie={}", ja_auth_cookie.trim())
        };
        for url in [CANVAS_BASE, AUTH_URL, MY_SJTU_URL] {
            if let Ok(parsed) = url.parse() {
                jar.add_cookie_str(&cookie_pair, &parsed);
            }
        }

        let client = reqwest::Client::builder()
            .cookie_provider(Arc::clone(&jar))
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                 AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/131.0.0.0 Safari/537.36",
            )
            .build()
            .expect("Failed to build HTTP client");

        Self {
            course_id,
            cookies: jar,
            client,
            session: None,
        }
    }

    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------

    /// Full 5-step LTI/OIDC auth flow.
    ///
    /// Returns a [`CanvasSession`] on success that can be used for subsequent
    /// API calls.
    pub async fn authenticate(&mut self) -> Result<CanvasSession> {
        // Match SJTU-Canvas-Helper's Canvas login warm-up. This follows the
        // SSO redirects with the seeded JAAuthCookie before launching LTI.
        let login_resp = self
            .client
            .get(CANVAS_LOGIN_URL)
            .send()
            .await
            .context("Canvas login warm-up request failed")?;
        if login_resp
            .url()
            .domain()
            .is_some_and(|domain| domain == "jaccount.sjtu.edu.cn")
        {
            anyhow::bail!("Canvas login warm-up redirected to jAccount; JAAuthCookie may be invalid or expired");
        }

        // Step 1 - GET the Canvas external tool page.
        let ext_url = format!(
            "{}{}",
            CANVAS_BASE,
            EXTERNAL_TOOL_PATH.replace("{course_id}", &self.course_id)
        );
        let resp1 = self
            .client
            .get(&ext_url)
            .send()
            .await
            .context("Failed to fetch Canvas external tool page")?;
        let html1 = resp1
            .text()
            .await
            .context("Failed to read external tool page body")?;

        // Step 2 - Parse first form -> POST to OIDC login_initiations.
        let inputs1 = extract_form_inputs_for_action(&html1, OIDC_ACTION)
            .context("Step 2: parsing OIDC form")?;
        let resp2 = self
            .client
            .post(OIDC_ACTION)
            .form(&inputs1)
            .send()
            .await
            .context("Step 2: POST to OIDC login_initiations failed")?;
        let html2 = resp2
            .text()
            .await
            .context("Step 2: reading OIDC response body failed")?;

        // Step 3 - Parse second form -> POST to LTI3 auth (do NOT follow redirect).
        let inputs2 = extract_form_inputs_for_action(&html2, LTI3_AUTH_ACTION)
            .context("Step 3: parsing LTI3 form")?;

        // Build a temporary client that shares our cookie jar but does NOT follow
        // redirects so we can inspect the Location header.
        let no_redirect_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .cookie_provider(Arc::clone(&self.cookies))
            .build()
            .context("Step 3: building no-redirect client failed")?;

        let resp3 = no_redirect_client
            .post(LTI3_AUTH_ACTION)
            .form(&inputs2)
            .send()
            .await
            .context("Step 3: POST to LTI3 auth failed")?;

        // Step 4 - Extract tokenId from Location header.
        let location = resp3
            .headers()
            .get(reqwest::header::LOCATION)
            .context("No Location header in LTI3 auth redirect")?
            .to_str()
            .context("Location header is not valid UTF-8")?;

        let token_id = extract_token_id(location)?;

        // Step 5 - Exchange tokenId for access token.
        let token_url = format!(
            "{}/jy-application-canvas-sjtu/lti3/getAccessTokenByTokenId?tokenId={}",
            VIDEO_BASE, token_id
        );
        let token_resp = self
            .client
            .get(&token_url)
            .send()
            .await
            .context("Step 5: getAccessTokenByTokenId request failed")?;
        let token_json: serde_json::Value = token_resp
            .json()
            .await
            .context("Step 5: parsing access token JSON failed")?;

        let canvas_course_id = token_json
            .get("data")
            .and_then(|d| {
                d.get("params")
                    .and_then(|p| p.get("courId"))
                    .or_else(|| d.get("canvasCourseId"))
            })
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .context("Could not extract canvas course ID")?;

        let token = token_json
            .get("data")
            .and_then(|d| d.get("token"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .context("Could not extract access token")?;

        Ok(CanvasSession {
            course_id: self.course_id.clone(),
            token,
            canvas_course_id,
        })
    }

    // -----------------------------------------------------------------------
    // Video listing
    // -----------------------------------------------------------------------

    /// List all videos for the course.
    pub async fn list_videos(&mut self) -> Result<Vec<CanvasVideoInfo>> {
        let (token, canvas_course_id) = {
            let session = self.ensure_session().await?;
            (session.token.clone(), session.canvas_course_id.clone())
        };

        let url = format!(
            "{}/jy-application-canvas-sjtu/directOnDemandPlay/findVodVideoList",
            VIDEO_BASE
        );
        let encoded_canvas_course_id: String =
            url::form_urlencoded::byte_serialize(canvas_course_id.as_bytes()).collect();
        let mut body = HashMap::new();
        body.insert("canvasCourseId", encoded_canvas_course_id);

        let resp = self
            .client
            .post(&url)
            .header("token", &token)
            .header(
                "Referer",
                "https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/",
            )
            .json(&body)
            .send()
            .await
            .context("Failed to fetch video list")?;

        let json: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse video list response")?;

        // Resolve the items array through multiple possible wrappers.
        let items = resolve_json_array(&json);

        let videos: Vec<CanvasVideoInfo> = items
            .iter()
            .filter_map(|item| {
                let video_id = item
                    .get("videoId")
                    .or_else(|| item.get("video_id"))
                    .or_else(|| item.get("id"))
                    .or_else(|| item.get("vodId"))
                    .and_then(value_to_string)?;

                let title = item
                    .get("videoName")
                    .or_else(|| item.get("video_name"))
                    .or_else(|| item.get("title"))
                    .or_else(|| item.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Untitled")
                    .to_string();

                let duration = item
                    .get("duration")
                    .or_else(|| item.get("videoDuration"))
                    .or_else(|| item.get("videPlayTime"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let course_begin_time = item
                    .get("courseBeginTime")
                    .or_else(|| item.get("course_begin_time"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let course_end_time = item
                    .get("courseEndTime")
                    .or_else(|| item.get("course_end_time"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let teacher = item
                    .get("userName")
                    .or_else(|| item.get("user_name"))
                    .or_else(|| item.get("teacher"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let classroom = item
                    .get("classroomName")
                    .or_else(|| item.get("classroom_name"))
                    .or_else(|| item.get("clroName"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                Some(CanvasVideoInfo {
                    video_id,
                    title,
                    duration,
                    course_begin_time,
                    course_end_time,
                    teacher,
                    classroom,
                })
            })
            .collect();

        Ok(videos)
    }

    // -----------------------------------------------------------------------
    // Subtitle fetching
    // -----------------------------------------------------------------------

    /// Fetch subtitles for a specific video.
    ///
    /// Returns a [`TranscriptArtifact`] containing the parsed subtitle
    /// segments.
    pub async fn fetch_subtitles(&mut self, video_id: &str) -> Result<TranscriptArtifact> {
        let (token, course_id_str) = {
            let session = self.ensure_session().await?;
            (session.token.clone(), session.course_id.clone())
        };

        // Step 1 - Get video info (courId, title).
        let info_url = format!(
            "{}/jy-application-canvas-sjtu/directOnDemandPlay/getVodVideoInfos",
            VIDEO_BASE
        );

        let form_data: [(&str, &str); 3] = [
            ("playTypeHls", "true"),
            ("id", video_id),
            ("isAudit", "true"),
        ];

        let info_resp = self
            .client
            .post(&info_url)
            .form(&form_data)
            .header("token", &token)
            .header(
                "Referer",
                "https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/",
            )
            .send()
            .await
            .context("Failed to fetch video info")?;

        let info_json: serde_json::Value = info_resp
            .json()
            .await
            .context("Failed to parse video info JSON")?;

        // Extract video title.
        let info_data = info_json.get("data").unwrap_or(&info_json);
        let video_title = info_data
            .get("videName")
            .or_else(|| info_data.get("vide_name"))
            .or_else(|| info_data.get("title"))
            .or_else(|| info_data.get("videoTitle"))
            .or_else(|| info_data.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled")
            .to_string();
        let recorded_at = info_data
            .get("videBeginTime")
            .or_else(|| info_data.get("vide_begin_time"))
            .or_else(|| info_data.get("courseBeginTime"))
            .or_else(|| info_data.get("course_begin_time"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Extract courId (may be at top level or inside `data`).
        let cour_id = info_data
            .get("courId")
            .or_else(|| info_data.get("cour_id"))
            .or_else(|| info_data.get("id"))
            .and_then(value_to_string)
            .context("Could not extract courId from video info")?;

        // Step 2 - Get translate/detail (subtitle data).
        let detail_url = format!(
            "{}/jy-application-canvas-sjtu/transfer/translate/detail",
            VIDEO_BASE
        );
        let detail_body = serde_json::json!({"courseId": &cour_id});

        let detail_resp = self
            .client
            .post(&detail_url)
            .json(&detail_body)
            .header("token", &token)
            .header(
                "Referer",
                "https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/",
            )
            .send()
            .await
            .context("Failed to fetch subtitle detail")?;

        let detail_json: serde_json::Value = detail_resp
            .json()
            .await
            .context("Failed to parse subtitle detail JSON")?;

        let segments = extract_subtitle_segments(&detail_json);

        Ok(TranscriptArtifact {
            video_id: video_id.to_string(),
            video_title,
            course_id: course_id_str,
            language: "zh".to_string(),
            segments,
            fetched_at: chrono::Utc::now().to_rfc3339(),
            recorded_at,
            source_url: None,
        })
    }

    /// Fetch PPT/computer screenshot slice timestamps for a specific video.
    ///
    /// SJTU-Canvas-Helper uses:
    /// `/directOnDemandPlay/vod-analysis/query-ppt-slice-es?ivsVideoId={courId}`.
    /// The returned `createSec` values are used here as paragraph boundaries
    /// for transcript stitching.
    pub async fn fetch_ppt_slices(&mut self, video_id: &str) -> Result<Vec<CanvasPptSlice>> {
        let token = {
            let session = self.ensure_session().await?;
            session.token.clone()
        };

        let info_url = format!(
            "{}/jy-application-canvas-sjtu/directOnDemandPlay/getVodVideoInfos",
            VIDEO_BASE
        );
        let form_data: [(&str, &str); 3] = [
            ("playTypeHls", "true"),
            ("id", video_id),
            ("isAudit", "true"),
        ];
        let info_resp = self
            .client
            .post(&info_url)
            .form(&form_data)
            .header("token", &token)
            .header(
                "Referer",
                "https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/",
            )
            .send()
            .await
            .context("Failed to fetch video info for PPT slices")?;

        let info_json: serde_json::Value = info_resp
            .json()
            .await
            .context("Failed to parse video info JSON for PPT slices")?;
        let info_data = info_json.get("data").unwrap_or(&info_json);
        let cour_id = info_data
            .get("courId")
            .or_else(|| info_data.get("cour_id"))
            .or_else(|| info_data.get("id"))
            .and_then(value_to_string)
            .context("Could not extract courId from video info for PPT slices")?;

        let ppt_url = format!(
            "{}/jy-application-canvas-sjtu/directOnDemandPlay/vod-analysis/query-ppt-slice-es?ivsVideoId={}",
            VIDEO_BASE, cour_id
        );
        let ppt_resp = self
            .client
            .get(&ppt_url)
            .header("token", &token)
            .header(
                "Referer",
                "https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/",
            )
            .send()
            .await
            .context("Failed to fetch PPT slices")?;

        let ppt_json: serde_json::Value = ppt_resp
            .json()
            .await
            .context("Failed to parse PPT slice JSON")?;
        Ok(extract_ppt_slices(&ppt_json))
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Lazy-initialise the session (authenticate once, reuse thereafter).
    async fn ensure_session(&mut self) -> Result<&CanvasSession> {
        if self.session.is_none() {
            let session = self.authenticate().await?;
            self.session = Some(session);
        }
        Ok(self.session.as_ref().unwrap())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers (module-level)
// ---------------------------------------------------------------------------

/// Resolve a potentially-relative URL against a base.
#[cfg(test)]
fn resolve_url(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') {
        // Absolute path - extract origin from base and append.
        if let Ok(parsed) = url::Url::parse(base) {
            format!(
                "{}://{}{}",
                parsed.scheme(),
                parsed.host_str().unwrap_or(""),
                url
            )
        } else {
            format!("{}{}", base.trim_end_matches('/'), url)
        }
    } else {
        // Relative path.
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            url.trim_start_matches('/')
        )
    }
}

/// Walk common wrapper keys to dig out a JSON array.
///
/// Tries `data` / `result` at the top level, then `records` / `list` nested
/// inside those, and finally `records` / `list` directly on the top-level
/// value.
fn resolve_json_array(json: &serde_json::Value) -> Vec<serde_json::Value> {
    // Top-level array.
    if let Some(arr) = json.as_array() {
        return arr.clone();
    }

    // Under `data` or `result`.
    for wrapper in &["data", "result"] {
        if let Some(inner) = json.get(wrapper) {
            if let Some(arr) = inner.as_array() {
                return arr.clone();
            }
            // Under `data.records`, `data.list`, etc.
            for list_key in &["records", "list"] {
                if let Some(arr) = inner.get(list_key).and_then(|v| v.as_array()) {
                    return arr.clone();
                }
            }
        }
    }

    // Direct `records` or `list`.
    for list_key in &["records", "list"] {
        if let Some(arr) = json.get(list_key).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }

    Vec::new()
}

fn parse_seconds_value(v: &serde_json::Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        Some(n)
    } else if let Some(s) = v.as_str() {
        s.trim().parse::<f64>().ok()
    } else {
        None
    }
}

fn extract_ppt_slices(json: &serde_json::Value) -> Vec<CanvasPptSlice> {
    let mut slices: Vec<CanvasPptSlice> = resolve_json_array(json)
        .iter()
        .filter_map(|item| {
            let create_sec = item
                .get("createSec")
                .or_else(|| item.get("create_sec"))
                .or_else(|| item.get("time"))
                .or_else(|| item.get("timestamp"))
                .and_then(parse_seconds_value)?;
            let ppt_img_url = item
                .get("pptImgUrl")
                .or_else(|| item.get("ppt_img_url"))
                .or_else(|| item.get("imgUrl"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let ocr_words = item
                .get("ocr")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|ocr| ocr.get("word").and_then(|v| v.as_str()))
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(CanvasPptSlice {
                create_sec,
                ppt_img_url,
                ocr_words,
            })
        })
        .collect();
    slices.sort_by(|a, b| a.create_sec.total_cmp(&b.create_sec));
    slices.dedup_by(|a, b| (a.create_sec - b.create_sec).abs() < 0.001);
    slices
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // extract_token_id
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_token_id_query_param() {
        let location = "https://example.com/lti3?tokenId=abc123&other=stuff";
        let token = extract_token_id(location).unwrap();
        assert_eq!(token, "abc123");
    }

    #[test]
    fn test_extract_token_id_path_segment() {
        let location = "https://example.com/auth/tokenId/xyz789?foo=bar";
        let token = extract_token_id(location).unwrap();
        assert_eq!(token, "xyz789");
    }

    #[test]
    fn test_extract_token_id_equals_in_path() {
        // Regex fallback catches `tokenId=` even without proper query delimiters.
        let location = "https://example.com/lti3?tokenId=deadbeef";
        let token = extract_token_id(location).unwrap();
        assert_eq!(token, "deadbeef");
    }

    #[test]
    fn test_extract_token_id_no_match() {
        let location = "https://example.com/no/token/here?other=1";
        let result = extract_token_id(location);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_token_id_empty() {
        let result = extract_token_id("");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_ppt_slices_from_data() {
        let data = json!({
            "data": [
                {
                    "createSec": "12.5",
                    "pptImgUrl": "https://example.com/1.jpg",
                    "ocr": [{"word": "Alpha"}, {"word": "Beta"}]
                },
                {
                    "createSec": 30,
                    "pptImgUrl": "https://example.com/2.jpg",
                    "ocr": []
                }
            ]
        });

        let slices = extract_ppt_slices(&data);

        assert_eq!(slices.len(), 2);
        assert!((slices[0].create_sec - 12.5).abs() < 0.001);
        assert_eq!(
            slices[0].ppt_img_url.as_deref(),
            Some("https://example.com/1.jpg")
        );
        assert_eq!(
            slices[0].ocr_words,
            vec!["Alpha".to_string(), "Beta".to_string()]
        );
        assert!((slices[1].create_sec - 30.0).abs() < 0.001);
    }

    // -----------------------------------------------------------------------
    // extract_form_action_and_inputs
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_form_standard() {
        let html = r#"
        <html>
        <body>
        <form action="https://v.sjtu.edu.cn/oidc/login_initiations" method="post">
            <input type="hidden" name="iss" value="https://oc.sjtu.edu.cn" />
            <input type="hidden" name="login_hint" value="user123" />
            <input type="submit" value="Go" />
        </form>
        </body>
        </html>
        "#;

        let (action, inputs) = extract_form_action_and_inputs(html).unwrap();
        assert_eq!(action, "https://v.sjtu.edu.cn/oidc/login_initiations");
        assert_eq!(inputs.len(), 2);
        // Check we got the expected key-value pairs.
        let names: Vec<&str> = inputs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"iss"));
        assert!(names.contains(&"login_hint"));
        // The submit input has no name - it should NOT appear.
    }

    #[test]
    fn test_extract_form_input_value_before_name() {
        // Attributes in reverse order should still parse correctly.
        let html = r#"
        <form action="/login">
            <input value="val1" name="key1" />
            <input value="val2" name="key2" />
        </form>
        "#;

        let (action, inputs) = extract_form_action_and_inputs(html).unwrap();
        assert_eq!(action, "/login");
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0], ("key1".into(), "val1".into()));
        assert_eq!(inputs[1], ("key2".into(), "val2".into()));
    }

    #[test]
    fn test_extract_form_inputs_for_exact_action() {
        let html = r#"
        <form action="https://example.com/wrong">
            <input name="bad" value="1" />
        </form>
        <form action="https://v.sjtu.edu.cn/jy-application-canvas-sjtu/lti3/lti3Auth/ivs">
            <input name="state" value="abc" />
            <input name="nonce" value="xyz" />
            <input name="submit_without_value" />
        </form>
        "#;

        let inputs = extract_form_inputs_for_action(html, LTI3_AUTH_ACTION).unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0], ("state".into(), "abc".into()));
        assert_eq!(inputs[1], ("nonce".into(), "xyz".into()));
    }

    #[test]
    fn test_extract_form_inputs_for_exact_action_missing() {
        let html = r#"<form action="https://example.com/wrong"></form>"#;
        let result = extract_form_inputs_for_action(html, LTI3_AUTH_ACTION);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_form_missing_action() {
        let html = r#"<form><input name="x" value="y" /></form>"#;
        let result = extract_form_action_and_inputs(html);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_form_no_form_element() {
        let html = r#"<html><body><p>No form here</p></body></html>"#;
        let result = extract_form_action_and_inputs(html);
        assert!(result.is_err());
    }

    #[test]
    fn test_value_to_string_accepts_strings_and_numbers() {
        assert_eq!(value_to_string(&json!("42")).unwrap(), "42");
        assert_eq!(value_to_string(&json!(42)).unwrap(), "42");
        assert_eq!(value_to_string(&json!(42_u64)).unwrap(), "42");
    }

    // -----------------------------------------------------------------------
    // segments_from_canvas_subtitles
    // -----------------------------------------------------------------------

    #[test]
    fn test_segments_from_standard_sjtu_items() {
        let items = vec![
            json!({"bg": 0, "ed": 5000, "res": "Hello world"}),
            json!({"bg": 5000, "ed": 10000, "res": "Second sentence"}),
        ];

        let segments = segments_from_canvas_subtitles(&items);
        assert_eq!(segments.len(), 2);

        assert_eq!(segments[0].index, 1);
        assert!((segments[0].start_time - 0.0).abs() < 0.001);
        assert!((segments[0].end_time - 5.0).abs() < 0.001);
        assert_eq!(segments[0].text, "Hello world");

        assert_eq!(segments[1].index, 2);
        assert!((segments[1].start_time - 5.0).abs() < 0.001);
        assert!((segments[1].end_time - 10.0).abs() < 0.001);
        assert_eq!(segments[1].text, "Second sentence");
    }

    #[test]
    fn test_segments_with_alternative_field_names() {
        let items = vec![
            json!({"startTime": 1000, "endTime": 3000, "oneSentence": "Alt fields"}),
            json!({"start": 3000, "end": 6000, "res": "Third sentence"}),
        ];

        let segments = segments_from_canvas_subtitles(&items);
        assert_eq!(segments.len(), 2);
        assert!((segments[0].start_time - 1.0).abs() < 0.001);
        assert!((segments[0].end_time - 3.0).abs() < 0.001);
        assert_eq!(segments[0].text, "Alt fields");
        assert_eq!(segments[1].text, "Third sentence");
    }

    #[test]
    fn test_segments_skips_empty_text() {
        let items = vec![
            json!({"bg": 0, "ed": 1000, "res": ""}),
            json!({"bg": 1000, "ed": 2000, "res": "  "}),
            json!({"bg": 2000, "ed": 3000, "res": "Real text"}),
        ];

        let segments = segments_from_canvas_subtitles(&items);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Real text");
        assert_eq!(segments[0].index, 3); // 1-based index preserved.
    }

    #[test]
    fn test_segments_default_end_time() {
        // When only bg is provided, ed defaults to bg + 1000 ms.
        let items = vec![json!({"bg": 2000, "res": "No end"})];

        let segments = segments_from_canvas_subtitles(&items);
        assert_eq!(segments.len(), 1);
        assert!((segments[0].start_time - 2.0).abs() < 0.001);
        assert!((segments[0].end_time - 3.0).abs() < 0.001);
    }

    // -----------------------------------------------------------------------
    // extract_subtitle_text
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_subtitle_text_string_input() {
        let data = json!("Plain subtitle text");
        let text = extract_subtitle_text(&data);
        assert_eq!(text, "Plain subtitle text");
    }

    #[test]
    fn test_extract_subtitle_text_dict_with_known_key() {
        let data = json!({"subtitles": "SRT content here"});
        let text = extract_subtitle_text(&data);
        assert_eq!(text, "SRT content here");
    }

    #[test]
    fn test_extract_subtitle_text_dict_with_content_key() {
        let data = json!({"content": "Some content text"});
        let text = extract_subtitle_text(&data);
        assert_eq!(text, "Some content text");
    }

    #[test]
    fn test_extract_subtitle_text_nested_list_fallback() {
        let data = json!({
            "records": [
                {"bg": 0, "ed": 2000, "res": "Line one"},
                {"bg": 2000, "ed": 4000, "res": "Line two"}
            ]
        });
        let text = extract_subtitle_text(&data);
        assert!(text.contains("Line one"));
        assert!(text.contains("Line two"));
    }

    #[test]
    fn test_extract_subtitle_text_fallback_pretty() {
        let data = json!({"unknown_key": [1, 2, 3]});
        let text = extract_subtitle_text(&data);
        // Should contain the pretty-printed JSON.
        assert!(text.contains("unknown_key"));
    }

    // -----------------------------------------------------------------------
    // extract_subtitle_segments
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_subtitle_segments_from_before_assembly_list() {
        let data = json!({
            "beforeAssemblyList": [
                {"bg": 0, "ed": 3000, "res": "Segment one"},
                {"bg": 3000, "ed": 6000, "res": "Segment two"}
            ]
        });
        let segments = extract_subtitle_segments(&data);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "Segment one");
        assert_eq!(segments[1].text, "Segment two");
    }

    #[test]
    fn test_extract_subtitle_segments_from_after_assembly_list() {
        let data = json!({
            "afterAssemblyList": [
                {"bg": 0, "ed": 5000, "res": "After segment"}
            ]
        });
        let segments = extract_subtitle_segments(&data);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "After segment");
    }

    #[test]
    fn test_extract_subtitle_segments_from_nested_data() {
        let data = json!({
            "data": {
                "beforeAssemblyList": [
                    {"bg": 0, "ed": 4000, "res": "Nested segment"}
                ]
            }
        });
        let segments = extract_subtitle_segments(&data);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Nested segment");
    }

    #[test]
    fn test_extract_subtitle_segments_empty() {
        let data = json!({"nothing": "useful"});
        let segments = extract_subtitle_segments(&data);
        assert!(segments.is_empty());
    }

    // -----------------------------------------------------------------------
    // resolve_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_url_absolute() {
        let result = resolve_url("https://v.sjtu.edu.cn", "https://other.example.com/path");
        assert_eq!(result, "https://other.example.com/path");
    }

    #[test]
    fn test_resolve_url_absolute_path() {
        let result = resolve_url("https://v.sjtu.edu.cn", "/api/endpoint");
        assert_eq!(result, "https://v.sjtu.edu.cn/api/endpoint");
    }

    #[test]
    fn test_resolve_url_relative() {
        let result = resolve_url("https://v.sjtu.edu.cn/base", "sub/path");
        assert_eq!(result, "https://v.sjtu.edu.cn/base/sub/path");
    }

    // -----------------------------------------------------------------------
    // resolve_json_array
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_json_array_top_level() {
        let json = json!([1, 2, 3]);
        let items = resolve_json_array(&json);
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn test_resolve_json_array_under_data() {
        let json = json!({"data": [{"id": "a"}, {"id": "b"}]});
        let items = resolve_json_array(&json);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_resolve_json_array_under_data_records() {
        let json = json!({"data": {"records": [{"x": 1}]}});
        let items = resolve_json_array(&json);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn test_resolve_json_array_missing() {
        let json = json!({"unknown": "stuff"});
        let items = resolve_json_array(&json);
        assert!(items.is_empty());
    }
}
