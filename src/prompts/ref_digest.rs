//! Reference Digest prompt builders.
//!
//! System and user prompts for generating a comprehensive Reference Digest
//! from lecture transcripts.

use crate::web::sources::truncate_for_llm;

/// Build the system prompt for Reference Digest generation.
pub(crate) fn build_ref_digest_system_prompt() -> String {
    "\
You are a lecture digest writer. Create a detailed, structured Markdown Reference Digest from \
lecture transcripts. The digest will be used downstream to compress into an exam cheat sheet, \
so be comprehensive and precise.\n\n\
Goal: detailed, structured Markdown covering definitions, formulas, conditions, algorithms, \
steps, comparisons, pitfalls, exam judgement rules, and timestamp evidence.\n\n\
Default language: Chinese, preserving English technical terms, identifiers, symbols, and formulas.\n\
Use ## for top-level sections and ### for subsections.\n\
Do not invent facts beyond the supplied sources.\n\
Include [MM:SS] timestamps when referencing specific moments.\n\
Return ONLY Markdown, no code fences, no explanations.".to_string()
}

/// Build the user prompt for Reference Digest generation.
pub(crate) fn build_ref_digest_user_prompt(
    note_content: &Option<String>,
    transcript_context: &str,
    _mode: Option<&str>,
) -> String {
    let mut user = String::new();

    if let Some(ref note) = note_content {
        user.push_str(&format!(
            "Reference Note (structure / priority / style reference only; not a length constraint):\n\n{}\n\n---\n\n",
            truncate_for_llm(note, 50000)
        ));
    }

    user.push_str(&format!(
        "Transcript sources:\n\n{}\n\n---\n\n\
        Create a comprehensive Reference Digest Markdown document. \
        Prioritize: definitions, formulas, conditions, algorithms, steps, comparisons, \
        pitfalls, exam judgement rules, and timestamp evidence. \
        Be detailed and precise - this will be further compressed into an exam cheat sheet. \
        Return ONLY Markdown.",
        transcript_context
    ));

    user
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reference_digest_prompt_uses_note_as_structure_reference() {
        let note = Some("# My Notes\n\nSome content.".to_string());
        let prompt = build_ref_digest_user_prompt(&note, "transcript context", None);
        assert!(prompt.contains("My Notes"));
        assert!(prompt.contains("transcript context"));
    }
}
