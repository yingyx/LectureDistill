//! Note patch prompt builder.
//!
//! Builds system and user prompts for patching Markdown notes with
//! transcript context.

/// Build system and user prompts for note patching.
///
/// Returns `(system_prompt, user_prompt)`.
pub(crate) fn build_note_patch_prompts(
    has_base_note: bool,
    context: &str,
    context_limit: usize,
) -> (String, String) {
    let system_prompt = if has_base_note {
        "You are an expert note editor. You are given an existing Markdown note and supplementary source materials (lecture transcripts, etc.). \
         Your job is to produce a COMPLETE updated Markdown note that incorporates key information from the sources into the existing note. \
         Make only the SMALLEST necessary modifications -- preserve the existing structure, wording, and formatting wherever possible. \
         Add missing details, correct factual errors, and supplement with important information from the sources. \
         Return ONLY the complete updated Markdown note, with no surrounding explanation, no code fences, and no commentary. \
         The output MUST be valid Markdown and MUST be the full note, not just the changes."
    } else {
        "You are an expert note writer. You are given lecture transcript materials and you need to generate a well-structured Markdown note. \
         Organize the content logically: group related topics, use headings (## for sections, ### for sub-sections), \
         include bullet points for key facts, and preserve timestamps in [MM:SS] format when citing specific moments. \
         Be comprehensive but well-organized. Do not invent facts not present in the sources. \
         Return ONLY the generated Markdown note, with no surrounding explanation, no code fences, and no commentary. \
         The output MUST be valid Markdown."
    };

    let context = if context.chars().count() > context_limit {
        let mut truncated = context.chars().take(context_limit).collect::<String>();
        truncated.push_str("\n\n[context truncated]");
        truncated
    } else {
        context.to_string()
    };

    let user_prompt = format!(
        "Source materials (lecture transcripts, etc.):\n\n{}\n\n\
         Produce the complete Markdown note now.",
        context
    );

    (system_prompt.to_string(), user_prompt)
}
