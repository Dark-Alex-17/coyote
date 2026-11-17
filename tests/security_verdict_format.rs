//! Pins the machine-read tail of the security reviewer's verdict. Callers read
//! the numbered findings under the literal `Blocking findings:` and
//! `Non-blocking findings:` headers that follow the last `SECURITY_REVIEW:`
//! line. A PASS template without a header, or a header the model wraps in bold,
//! leaves findings the reviewer reported invisible to every such reader. Each
//! template in the agent prompt, the skill and the README must carry the
//! headers, and the prompt and skill must say the tail is plain text.

use std::fs;
use std::path::Path;

const HEADERS: [&str; 2] = ["Blocking findings:", "Non-blocking findings:"];

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// The fenced template block that opens with `SECURITY_REVIEW: <verdict>`.
fn template(text: &str, verdict: &str) -> Vec<String> {
    let open = format!("SECURITY_REVIEW: {verdict}");
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim() == open)
        .unwrap_or_else(|| panic!("no {open} template"));
    lines[start..]
        .iter()
        .take_while(|l| !l.starts_with("```"))
        .map(|l| l.to_string())
        .collect()
}

/// Numbered items under a literal header after the last sentinel line: the
/// contract a caller relies on, with nothing tolerated beyond it. Items under
/// the trailing `Pre-existing, out of scope:` header are not findings.
fn reported(report: &str) -> Vec<String> {
    let lines: Vec<&str> = report.lines().collect();
    let Some(last) = lines
        .iter()
        .rposition(|l| l.starts_with("SECURITY_REVIEW:"))
    else {
        return Vec::new();
    };
    let mut in_section = false;
    let mut items = Vec::new();
    for line in &lines[last + 1..] {
        let stripped = line.trim();
        if stripped.starts_with("```") || stripped.starts_with("Pre-existing") {
            break;
        }
        if HEADERS.contains(&stripped) {
            in_section = true;
            continue;
        }
        let numbered = line
            .split_once(". ")
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if let (true, Some((_, rest))) = (in_section, numbered) {
            items.push(rest.to_string());
        }
    }
    items
}

fn assert_pass_template_has_header(rel: &str) {
    let block = template(&read(rel), "PASS");
    let header = block
        .iter()
        .position(|l| l == "Non-blocking findings:")
        .unwrap_or_else(|| {
            panic!("{rel}: the PASS template has no `Non-blocking findings:` line: {block:?}")
        });
    assert!(
        block.get(header + 1).is_some_and(|l| l.starts_with("1. ")),
        "{rel}: the PASS template's header is not followed by a numbered item: {block:?}"
    );
    assert_eq!(
        reported(&block.join("\n")).len(),
        1,
        "{rel}: a report shaped like the PASS template yields no finding"
    );
}

fn assert_fail_template_has_headers(rel: &str) {
    let block = template(&read(rel), "FAIL");
    for header in HEADERS {
        assert!(
            block.iter().any(|l| l == header),
            "{rel}: the FAIL template lost {header:?}: {block:?}"
        );
    }
    assert!(
        reported(&block.join("\n")).len() >= 2,
        "{rel}: a report shaped like the FAIL template yields fewer than two findings"
    );
}

#[test]
fn every_pass_template_lists_findings_under_a_header() {
    for rel in [
        "assets/agents/security-reviewer/instructions.md",
        "assets/skills/security-review/SKILL.md",
        "assets/agents/security-reviewer/README.md",
    ] {
        assert_pass_template_has_header(rel);
    }
}

#[test]
fn every_fail_template_keeps_both_headers() {
    for rel in [
        "assets/agents/security-reviewer/instructions.md",
        "assets/skills/security-review/SKILL.md",
        "assets/agents/security-reviewer/README.md",
    ] {
        assert_fail_template_has_headers(rel);
    }
}

#[test]
fn prompt_and_skill_say_the_tail_is_plain_text() {
    for rel in [
        "assets/agents/security-reviewer/instructions.md",
        "assets/skills/security-review/SKILL.md",
    ] {
        let text = read(rel).split_whitespace().collect::<Vec<_>>().join(" ");
        for anchor in [
            "emit them outside any code fence",
            "on its own line exactly as written, never bold or reworded",
            "list every finding that the Posture line counts",
            "An optional `Pre-existing, out of scope:` header always comes last, on its own plain line, with numbered items that the Posture line does not count.",
            "A finding that appears only in that prose is not reported.",
        ] {
            assert!(text.contains(anchor), "{rel} lost anchor {anchor:?}");
        }
        assert!(
            !text.contains("top 1-3 non-blocking findings"),
            "{rel}: the PASS template still caps the non-blocking findings it lists"
        );
    }
}

/// The three report shapes that lost reported findings in a recorded campaign,
/// reduced to their tails: a bold header, a numbered list straight under the
/// Posture line, and the same headerless list inside a code fence. Each counts
/// a finding, and none yields one to a caller; the shape the templates now
/// prescribe does, and its trailing out-of-scope items stay uncounted.
#[test]
fn recorded_off_contract_shapes_yield_nothing_and_the_prescribed_shape_does() {
    let posture = "Posture: standard. Findings: 0 critical, 1 high, 1 medium, 0 low.";
    let item = "1. \u{1f7e0} Secret in logs - app/server.py:194 - response bodies logged - log metadata only";
    let bold = format!("SECURITY_REVIEW: FAIL\n{posture}\n\n**Blocking findings:**\n{item}\n");
    let headerless = format!("SECURITY_REVIEW: PASS\n{posture}\n\n{item}\n");
    let fenced = format!("```\nSECURITY_REVIEW: PASS\n{posture}\n{item}\n```\n");
    for (shape, report) in [
        ("bold", &bold),
        ("headerless", &headerless),
        ("headerless inside a fence", &fenced),
    ] {
        assert!(
            reported(report).is_empty(),
            "{shape}: the reader is looser than the contract"
        );
    }
    let prescribed = format!(
        "SECURITY_REVIEW: PASS\n{posture}\nNon-blocking findings:\n{item}\n\
         Pre-existing, out of scope:\n1. Old weak hash - app/auth.py:12 - predates the change - migrate\n"
    );
    assert_eq!(
        reported(&prescribed),
        vec![item.trim_start_matches("1. ").to_string()]
    );
}
