//! Cheat Sheet prompt builders.
//!
//! Builds system and user prompts for cheat sheet generation (both standalone
//! and unified pipeline stages), expansion prompts, and metadata construction.

use crate::utils::budget::{
    build_section_inventory, compute_budget, count_effective, format_target,
    truncate_ref_digest_for_cheatsheet, ContentBudget, Language,
};
use crate::utils::calibration::CalibrationData;

const CHEAT_SHEET_MARKDOWN_RULES: &str = "\
Return ONLY a Markdown cheat sheet, with no code fences, no explanations, and no document preamble. \
The renderer will convert this Markdown into the fixed Typst template. \
Do NOT start with a whole-document title such as `# Reference Digest`, `# Cheat Sheet`, or `# Signals Reference Digest`; start directly with the first real content section. \
Use two content heading levels: `#` for real top-level sections and `##` for subsections. \
Allowed syntax only: headings using `#` and `##`; bullet lists using `-`; numbered lists using `1.`; \
LaTeX-style math delimited by `$...$`; inline code using backticks; bold terms using `**term**`; \
and simple horizontal rules using `---`. \
Do NOT emit Typst directives or macros such as `#set`, `#show`, `#let`, `#import`, `#include`, `#columns`, `#context`, `#metadata`, `#key[...]`, or `#cheatfact[...]`. \
Do NOT emit raw HTML, images, footnotes, tables, Mermaid, TikZ, or external packages. \
Use common LaTeX math syntax, e.g. `$\\frac{a}{b}$`, `$\\sqrt{x}$`, `$\\sum_{i=1}^n x_i$`, `$\\int_{-\\infty}^{\\infty} f(t)\\,dt$`, `$x \\ne 0$`, `$x \\le y$`, `$x \\to y$`. \
Inside math, insert explicit spaces or operators between adjacent variables, constants, and functions: write `$\\omega \\tau$`, `$f \\cos(n\\Omega t)$`, `$a_n \\cos(n\\Omega t)$`, `$j \\infty$`, `$j 0$`, and `$d\\tau$`/`$d t$`; do not write `$\\omega\\tau$`, `$f\\cos(...)$`, `$a_n\\cos(...)$`, `$j\\infty$`, or `$j0$`. \
Keep every `$...$` balanced.";

const CHEAT_SHEET_MARKDOWN_EXAMPLE: &str = "\
Example Markdown cheat sheet:\n\
# Signals\n\
## Key identities\n\
- **Linearity**: $a x(t) + b y(t) \\to a X(f) + b Y(f)$.\n\
- **Convolution**: time-domain convolution corresponds to frequency-domain multiplication.\n\
1. Check bandwidth $B$.\n\
2. Apply sampling condition $f_s > 2B$.\n\
---\n\
- **Exam rule**: If a system is LTI, first identify $h(t)$ and use $y(t)=x(t)*h(t)$.";

/// Build the Turn 2 user prompt: compress the Reference Digest into a cheat sheet.
///
/// The Reference Digest is NOT repeated here — it is already in the
/// conversation as the Turn 1 assistant response.
pub(crate) fn build_cheat_sheet_turn2_prompt(
    inventory: &str,
    section_names: &str,
    section_count: usize,
    max_pages: usize,
    budget: &ContentBudget,
) -> String {
    let target_text = format_target(budget);
    format!(
        "STAGE 2 — CHEAT SHEET.\n\n\
         Target: a {} page(s) exam cheat sheet.\n\
         {}. Up to roughly {} is acceptable — \
         the renderer will compress if needed.\n\n\
         Coverage requirement: the Reference Digest has {} main sections in order: {}\n\
         Every main section MUST contribute at least one of: definition, formula, \
         condition, algorithm step, pitfall, comparison, or exam judgement rule.\n\n\
         Section inventory:\n{}\n\n\
         Do not invent new facts.  Extract and organize the essential content from \
         the Reference Digest you created above.  Prefer completeness over conciseness.\n\n\
         {}\n\n\
         {}\n\n\
         Return only the complete cheating-sheet Markdown.  Aim for {}; more is acceptable.",
        max_pages,
        target_text,
        budget.soft_max,
        section_count,
        section_names,
        inventory,
        CHEAT_SHEET_MARKDOWN_RULES,
        CHEAT_SHEET_MARKDOWN_EXAMPLE,
        target_text,
    )
}

/// Build the Turn 3 user prompt: expand the cheat sheet with missing content.
///
/// Both the Reference Digest and the Cheat Sheet are already in the
/// conversation history — no excerpt or repetition needed.
pub(crate) fn build_expansion_turn3_prompt(
    target_add_chars: usize,
    inventory: &str,
    lang: Language,
) -> String {
    let unit = match lang {
        Language::Chinese => format!("{} 字", target_add_chars),
        Language::English => format!("{} words", target_add_chars / 5),
    };
    format!(
        "STAGE 3 — EXPANSION.\n\n\
         The cheat sheet you just created should be expanded with missing \
         high-value content from the Reference Digest you created earlier.  \
         Add approximately {} of high-value material.  \
         Maintain the exact same heading hierarchy and document structure.\n\n\
         Section inventory (all sections must remain covered):\n{}\n\n\
         Add the most exam-critical missing material: definitions, formulas, \
         conditions, pitfalls, algorithm steps, comparisons, and judgement \
         rules.  Do NOT add narrative, examples without reusable patterns, \
         or anything already covered.\n\n\
         {}\n\n\
         Return the complete expanded cheat-sheet Markdown.",
        unit, inventory, CHEAT_SHEET_MARKDOWN_RULES
    )
}

/// Build standalone cheat sheet prompts (used by both streaming and
/// non-streaming paths).
///
/// Now accepts calibration data for language-aware targeting.
///
/// Returns `(system_prompt, user_prompt)`.
pub(crate) fn build_cheat_sheet_prompts(
    ref_digest_markdown: &str,
    max_pages: usize,
    calib: &CalibrationData,
) -> (String, String) {
    let effective = count_effective(ref_digest_markdown);
    let budget = compute_budget(calib, &effective, max_pages);

    let (sections, inventory) = build_section_inventory(ref_digest_markdown);

    let section_count = sections.len();
    let section_list: Vec<String> = sections.iter().map(|s| s.heading.clone()).collect();
    let section_names = section_list.join(" | ");

    let target_text = format_target(&budget);

    let system_prompt = format!(
        "You convert a Reference Digest into an exam reference cheat-sheet Markdown document \
for a fixed renderer. Cover every topic comprehensively: for each section, include its essential \
definitions, formulas, conditions, algorithm steps, comparisons, pitfalls, and exam judgement rules. \
It is better to include slightly too much content than too little — the renderer will compress if \
needed. Do not omit topics.\n\
Use Chinese for Chinese source material, keep standard English technical terms, identifiers, and formulas. \
Prefer short headings, information-rich bullets, definitions, theorem/condition/result patterns, \
formulas, contrasts, and procedure steps.\n\
{}\n\n{}",
        CHEAT_SHEET_MARKDOWN_RULES, CHEAT_SHEET_MARKDOWN_EXAMPLE
    );

    let user_prompt = format!(
        "Target: a {} page(s) exam cheat sheet.\n\
{}. Up to roughly {} is acceptable — the renderer will compress if needed.\n\n\
Coverage requirement: the Reference Digest has {} main sections in order: {}\n\
Every main section MUST contribute at least one of: definition, formula, condition, algorithm step, \
pitfall, comparison, or exam judgement rule.\n\n\
Do not invent new facts. Extract and organize the essential content from the Reference Digest below. \
Prefer completeness over conciseness.\n\n\
Section inventory:\n{}\n\n\
Reference Digest Markdown:\n\n{}\n\n\
{}\n\n\
{}\n\n\
Return only the complete cheating-sheet Markdown. Aim for {}; more is acceptable.",
        max_pages,
        target_text,
        budget.soft_max,
        section_count,
        section_names,
        inventory,
        truncate_ref_digest_for_cheatsheet(ref_digest_markdown, 90000).0,
        CHEAT_SHEET_MARKDOWN_RULES,
        CHEAT_SHEET_MARKDOWN_EXAMPLE,
        target_text,
    );

    (system_prompt, user_prompt)
}

/// Build expansion prompts for standalone (non-unified) cheat sheet generation.
///
/// Returns `(system_prompt, user_prompt)`.
pub(crate) fn build_expansion_prompt(
    current_cheat: &str,
    section_inventory: &str,
    ref_digest_excerpt: &str,
    target_add_chars: usize,
    lang: Language,
) -> (String, String) {
    let unit = match lang {
        Language::Chinese => format!("{} 字", target_add_chars),
        Language::English => format!("{} words", target_add_chars / 5),
    };

    let system_prompt = format!(
        "You expand an existing exam cheat-sheet Markdown document by adding high-value material \
from the Reference Digest. Preserve the existing structure, headings, and content exactly as-is. \
Only add new material where it fits naturally: definitions, formulas, conditions, algorithm steps, \
pitfalls, comparisons, and exam judgement rules that are in the Reference Digest but missing from \
the cheat sheet. It is better to add slightly too much than too little — the renderer will compress \
if needed. \
Do not fabricate facts, do not rewrite existing sections, and do not change the document structure. \
{}\n\n{}",
        CHEAT_SHEET_MARKDOWN_RULES, CHEAT_SHEET_MARKDOWN_EXAMPLE
    );

    let user_prompt = format!(
        "The existing cheat sheet below should be expanded with missing high-value content \
from the Reference Digest. \
Add approximately {} of high-value material. \
Maintain the exact same heading hierarchy and document structure.\n\n\
Section inventory (all sections must remain covered):\n{}\n\n\
Existing cheat sheet Markdown:\n{}\n\n\
Reference Digest excerpt:\n{}\n\n\
Add the most exam-critical missing material: definitions, formulas, conditions, pitfalls, \
algorithm steps, comparisons, and judgement rules. Do NOT add narrative, examples without \
reusable patterns, or anything already covered. \
Return the complete expanded cheat-sheet Markdown.",
        unit, section_inventory, current_cheat, ref_digest_excerpt
    );

    (system_prompt, user_prompt)
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
            Language::Chinese,
        );
        assert!(sys.contains("expand"));
        assert!(user.contains("# Cheat"));
        // Chinese: "500 字"
        assert!(user.contains("500 字"));
    }

    #[test]
    fn test_expansion_prompt_contains_section_inventory() {
        let (_, user) = build_expansion_prompt(
            "# CS",
            "# Alpha\n  body: a\n# Beta\n  body: b\n",
            "# RD",
            200,
            Language::Chinese,
        );
        assert!(user.contains("Alpha"));
        assert!(user.contains("Beta"));
    }

    #[test]
    fn test_expansion_prompt_contains_target_add_chars() {
        let (_, user) =
            build_expansion_prompt("# CS", "# H\n  body: x\n", "# RD", 1234, Language::Chinese);
        // Chinese: "1234 字"
        assert!(user.contains("1234 字"));
    }

    #[test]
    fn test_expansion_prompt_contains_markdown_constraints() {
        let (sys, _) =
            build_expansion_prompt("# CS", "# H\n  body: x\n", "# RD", 100, Language::Chinese);
        assert!(sys.contains("Markdown"));
        assert!(sys.contains("LaTeX-style math"));
        assert!(sys.contains("**term**"));
        assert!(sys.contains("#set"));
        assert!(sys.contains("balanced"));
        assert!(sys.contains("whole-document title"));
        assert!(sys.contains("\\omega \\tau"));
        assert!(sys.contains("j0"));
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
