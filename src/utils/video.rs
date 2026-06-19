//! Canvas video grouping and date extraction utilities.

use std::collections::HashMap;

/// Group Canvas videos by date (YYYY-MM-DD), returning a JSON-ready array
/// sorted newest-first.
pub fn group_videos_by_date(
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
    extract_date_from_title(&v.title)
}

/// Try to extract a YYYY-MM-DD date from a video title string.
pub(crate) fn extract_date_from_title(title: &str) -> String {
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
    "unknown-date".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas_sjtu::CanvasVideoInfo;

    fn make_video(begin_time: &str, title: &str, video_id: &str) -> CanvasVideoInfo {
        CanvasVideoInfo {
            video_id: video_id.to_string(),
            title: title.to_string(),
            duration: 0,
            course_begin_time: begin_time.to_string(),
            course_end_time: String::new(),
            teacher: String::new(),
            classroom: String::new(),
        }
    }

    #[test]
    fn test_extract_date_from_begin_time() {
        let v = make_video("2023-09-18 08:00:00", "Lecture 1", "v1");
        assert_eq!(extract_date_from_video(&v), "2023-09-18");
    }

    #[test]
    fn test_extract_date_from_title_fallback() {
        let v = make_video("", "2023-09-18 Lecture Notes", "v2");
        assert_eq!(extract_date_from_video(&v), "2023-09-18");
    }

    #[test]
    fn test_extract_date_fallback_to_unknown() {
        let v = make_video("", "No date here", "v3");
        assert_eq!(extract_date_from_video(&v), "unknown-date");
    }

    #[test]
    fn test_group_videos_by_date() {
        let videos = vec![
            make_video("2023-09-18 08:00:00", "Lecture 1", "v1"),
            make_video("2023-09-18 10:00:00", "Lecture 2", "v2"),
            make_video("2023-09-19 08:00:00", "Lecture 3", "v3"),
        ];
        let groups = group_videos_by_date(&videos);
        assert_eq!(groups.len(), 2);
        // Sorted newest first: 2023-09-19 then 2023-09-18.
        assert_eq!(groups[0]["date"], "2023-09-19");
        assert_eq!(groups[0]["video_count"], 1);
        assert_eq!(groups[1]["date"], "2023-09-18");
        assert_eq!(groups[1]["video_count"], 2);
    }

    #[test]
    fn test_group_videos_empty() {
        let groups = group_videos_by_date(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_group_videos_single_date() {
        let videos = vec![make_video("2023-09-18 08:00:00", "L1", "v1")];
        let groups = group_videos_by_date(&videos);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["date"], "2023-09-18");
    }
}
