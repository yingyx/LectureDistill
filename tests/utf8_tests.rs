use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_MOJIBAKE_CODEPOINTS: &[u32] = &[
    0x9225, 0x9239, 0x9241, 0x923A, 0x6672, 0x6522, 0xE7FD, 0x5C16, 0xFFFD,
];

#[test]
fn source_files_do_not_contain_common_mojibake() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for rel in ["src", "tests", "web/src"] {
        collect_offenders(&root.join(rel), &mut offenders);
    }
    assert!(
        offenders.is_empty(),
        "common mojibake characters found in:\n{}",
        offenders.join("\n")
    );
}

fn collect_offenders(path: &Path, offenders: &mut Vec<String>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect_offenders(&entry.path(), offenders);
        }
        return;
    }

    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return;
    };
    if !matches!(ext, "rs" | "ts" | "tsx" | "css" | "md") {
        return;
    }
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    if content
        .chars()
        .any(|ch| FORBIDDEN_MOJIBAKE_CODEPOINTS.contains(&(ch as u32)))
    {
        offenders.push(path.display().to_string());
    }
}
