//! Cheat Sheet prompt builders.
//!
//! Builds system and user prompts for cheat sheet generation (both standalone
//! and unified pipeline stages), expansion prompts, and metadata construction.

use crate::utils::budget::{
    build_section_inventory, estimate_cheating_sheet_budget, truncate_ref_digest_for_cheatsheet,
    CheatingSheetBudget,
};

/// Build the Turn 2 user prompt: compress the Reference Digest into a cheat sheet.
///
/// The Reference Digest is NOT repeated here — it is already in the
/// conversation as the Turn 1 assistant response.
pub(crate) fn build_cheat_sheet_turn2_prompt(
    inventory: &str,
    section_names: &str,
    section_count: usize,
    max_pages: usize,
    budget: &CheatingSheetBudget,
) -> String {
    format!(
        "STAGE 2 — CHEAT SHEET.\n\n\
         Target: a {} page(s) exam cheat sheet.\n\
         Roughly {} characters typically fills {} page(s); up to {} is fine — \
         the renderer will compress if needed.\n\n\
         Coverage requirement: the Reference Digest has {} main sections in order: {}\n\
         Every main section MUST contribute at least one of: definition, formula, \
         condition, algorithm step, pitfall, comparison, or exam judgement rule.\n\n\
         Section inventory:\n{}\n\n\
         Do not invent new facts.  Extract and organize the essential content from \
         the Reference Digest you created above.  Prefer completeness over conciseness.\n\n\
         Return only the complete cheating-sheet Markdown.  Aim for roughly {} \
         characters; more is acceptable.",
        max_pages,
        budget.target_chars,
        max_pages,
        budget.soft_max_chars,
        section_count,
        section_names,
        inventory,
        budget.target_chars,
    )
}

/// Build the Turn 3 user prompt: expand the cheat sheet with missing content.
///
/// Both the Reference Digest and the Cheat Sheet are already in the
/// conversation history — no excerpt or repetition needed.
pub(crate) fn build_expansion_turn3_prompt(target_add_chars: usize, inventory: &str) -> String {
    format!(
        "STAGE 3 — EXPANSION.\n\n\
         The cheat sheet you just created should be expanded with missing \
         high-value content from the Reference Digest you created earlier.  \
         Add approximately {} more characters of high-value material.  \
         Maintain the exact same heading hierarchy and document structure.\n\n\
         Section inventory (all sections must remain covered):\n{}\n\n\
         Add the most exam-critical missing material: definitions, formulas, \
         conditions, pitfalls, algorithm steps, comparisons, and judgement \
         rules.  Do NOT add narrative, examples without reusable patterns, \
         or anything already covered.\n\n\
         Return the complete expanded cheat-sheet Markdown.",
        target_add_chars, inventory
    )
}

/// Build standalone cheat sheet prompts (used by both streaming and
/// non-streaming paths).
///
/// Returns `(system_prompt, user_prompt)`.
pub(crate) fn build_cheat_sheet_prompts(
    ref_digest_markdown: &str,
    max_pages: usize,
) -> (String, String) {
    let budget = estimate_cheating_sheet_budget(max_pages);
    let (sections, inventory) = build_section_inventory(ref_digest_markdown);

    let section_count = sections.len();
    let section_list: Vec<String> = sections.iter().map(|s| s.heading.clone()).collect();
    let section_names = section_list.join(" | ");

    let system_prompt = "You convert a Reference Digest into an exam reference cheat-sheet Markdown \
for a fixed LaTeX template. Cover every topic comprehensively: for each section, include its essential \
definitions, formulas, conditions, algorithm steps, comparisons, pitfalls, and exam judgement rules. \
It is better to include slightly too much content than too little — the renderer will compress if \
needed. Do not omit topics.\n\
Return ONLY Markdown, with no code fences, no explanations, and no raw LaTeX commands except ordinary \
math delimited by $...$ or $$...$$. \
The Markdown will be escaped and inserted into a XeLaTeX/xeCJK four-column A4 template, so avoid \
syntax that commonly breaks LaTeX: no HTML, images, footnotes, Markdown tables, nested tables, \
Mermaid, TikZ, custom macros, \\begin blocks, or unbalanced braces. \
Use Chinese for Chinese source material, keep standard English technical terms, identifiers, and formulas. \
Prefer short headings, information-rich bullets, definitions, theorem/condition/result patterns, \
formulas, contrasts, and procedure steps. \
Use only #, ##, ### headings, -, 1. lists, inline code for identifiers, bold for key terms, \
and standard Markdown math. \
Every formula must be syntactically balanced. Keep underscores and percent signs inside math or code \
when possible.";

    let user_prompt = format!(
        "Target: a {} page(s) exam cheat sheet.\n\
Roughly {} characters typically fills {} page(s); up to {} is fine — the renderer will \
compress if needed.\n\n\
Coverage requirement: the Reference Digest has {} main sections in order: {}\n\
Every main section MUST contribute at least one of: definition, formula, condition, algorithm step, \
pitfall, comparison, or exam judgement rule.\n\n\
Do not invent new facts. Extract and organize the essential content from the Reference Digest below. \
Prefer completeness over conciseness.\n\n\
Section inventory:\n{}\n\n\
Reference Digest Markdown:\n\n{}\n\n\
Return only the complete cheating-sheet Markdown. Aim for roughly {} characters; more is acceptable.",
        max_pages,
        budget.target_chars,
        max_pages,
        budget.soft_max_chars,
        section_count,
        section_names,
        inventory,
        truncate_ref_digest_for_cheatsheet(ref_digest_markdown, 90000).0,
        budget.target_chars,
    );

    (system_prompt.to_string(), user_prompt)
}

/// Build expansion prompts for standalone (non-unified) cheat sheet generation.
///
/// Returns `(system_prompt, user_prompt)`.
pub(crate) fn build_expansion_prompt(
    current_cheat: &str,
    section_inventory: &str,
    ref_digest_excerpt: &str,
    target_add_chars: usize,
) -> (String, String) {
    let system_prompt = "You expand an existing exam cheat-sheet Markdown by adding high-value material \
from the Reference Digest. Preserve the existing structure, headings, and content exactly as-is. \
Only add new material where it fits naturally: definitions, formulas, conditions, algorithm steps, \
pitfalls, comparisons, and exam judgement rules that are in the Reference Digest but missing from \
the cheat sheet. It is better to add slightly too much than too little — the renderer will compress \
if needed. \
Do not fabricate facts, do not rewrite existing sections, and do not change the document structure. \
Return ONLY Markdown, with no code fences, no explanations, and no raw LaTeX commands except ordinary \
math delimited by $...$ or $$...$$. The Markdown will be inserted into a XeLaTeX/xeCJK four-column A4 \
template, so avoid syntax that commonly breaks LaTeX: no HTML, images, footnotes, Markdown tables, \
nested tables, Mermaid, TikZ, custom macros, \\begin blocks, or unbalanced braces. \
Use only #, ##, ### headings, -, 1. lists, inline code for identifiers, bold for key terms, \
and standard Markdown math.";

    let user_prompt = format!(
        "The existing cheat sheet below should be expanded with missing high-value content \
from the Reference Digest. \
Add approximately {} more characters of high-value material. \
Maintain the exact same heading hierarchy and document structure.\n\n\
Section inventory (all sections must remain covered):\n{}\n\n\
Existing cheat sheet:\n{}\n\n\
Reference Digest excerpt:\n{}\n\n\
Add the most exam-critical missing material: definitions, formulas, conditions, pitfalls, \
algorithm steps, comparisons, and judgement rules. Do NOT add narrative, examples without \
reusable patterns, or anything already covered. \
Return the complete expanded cheat-sheet Markdown.",
        target_add_chars, section_inventory, current_cheat, ref_digest_excerpt
    );

    (system_prompt.to_string(), user_prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expansion_prompt_contains_current_cheat() {
        let (sys, user) = build_expansion_prompt(
            "# Cheat\n\ncontent",
            "# Cheat\n  body: content\n",
            "# RD\n\nrd content",
            500,
        );
        assert!(sys.contains("expand"));
        assert!(user.contains("# Cheat"));
        assert!(user.contains("500"));
    }

    #[test]
    fn test_expansion_prompt_contains_section_inventory() {
        let (_, user) = build_expansion_prompt(
            "# CS",
            "# Alpha\n  body: a\n# Beta\n  body: b\n",
            "# RD",
            200,
        );
        assert!(user.contains("Alpha"));
        assert!(user.contains("Beta"));
    }

    #[test]
    fn test_expansion_prompt_contains_target_add_chars() {
        let (_, user) = build_expansion_prompt("# CS", "# H\n  body: x\n", "# RD", 1234);
        assert!(user.contains("1234"));
    }

    #[test]
    fn test_expansion_prompt_contains_latex_safety_constraints() {
        let (sys, _) = build_expansion_prompt("# CS", "# H\n  body: x\n", "# RD", 100);
        assert!(sys.contains("XeLaTeX"));
        assert!(sys.contains("unbalanced braces"));
        // LaTeX safety: the system prompt should mention LaTeX and brace safety.
        assert!(sys.contains("LaTeX") || sys.contains("latex"));
    }

    #[test]
    fn test_metadata_includes_target_generated_harness_expansion_fields() {
        let meta = crate::utils::budget::build_cheatsheet_metadata(
            3,
            4,
            "rendering",
            2,
            2,
            1,
            "cheatsheet.tex",
            "md.md",
            "rd-id",
            22000,
            19500,
            1,
            false,
            2,
            None,
            None,
        );
        assert_eq!(meta["progress_current"], 3);
        assert_eq!(meta["target_chars"], 22000);
        assert_eq!(meta["generated_chars"], 19500);
        assert_eq!(meta["harness_attempts"], 1);
        assert_eq!(meta["expansion_used"], false);
        assert_eq!(meta["final_page_count"], 2);
        assert!(meta.get("underfilled_reason").is_none());
    }

    #[test]
    fn test_metadata_includes_underfilled_reason_when_present() {
        let meta = crate::utils::budget::build_cheatsheet_metadata(
            1,
            1,
            "",
            1,
            0,
            0,
            "",
            "",
            "",
            1000,
            500,
            0,
            false,
            1,
            Some("source too short"),
            None,
        );
        assert_eq!(meta["underfilled_reason"], "source too short");
    }

    #[test]
    fn test_metadata_omits_underfilled_reason_when_none() {
        let meta = crate::utils::budget::build_cheatsheet_metadata(
            1, 1, "", 1, 0, 0, "", "", "", 1000, 500, 0, false, 1, None, None,
        );
        assert!(meta.get("underfilled_reason").is_none());
    }

    #[test]
    fn test_metadata_preserves_existing_keys() {
        let meta = crate::utils::budget::build_cheatsheet_metadata(
            5, 5, "done", 3, 3, 0, "t.tex", "m.md", "rd-x", 33000, 34000, 1, true, 3, None, None,
        );
        assert_eq!(meta["max_pages"], 3);
        assert_eq!(meta["page_count"], 3);
        assert_eq!(meta["template_used"], "t.tex");
        assert_eq!(meta["reference_digest_output_id"], "rd-x");
    }

    #[test]
    fn test_expansion_used_metadata_true() {
        let meta = crate::utils::budget::build_cheatsheet_metadata(
            0, 0, "", 0, 0, 0, "", "", "", 0, 0, 0, true, 0, None, None,
        );
        assert_eq!(meta["expansion_used"], true);
    }
}
