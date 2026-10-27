use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::TemplateVar;

pub fn content_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn render_template(
    source: &str,
    vars: &[TemplateVar],
    answers: &BTreeMap<String, String>,
) -> String {
    let mut rendered = source.to_owned();
    for var in vars {
        let value = answers
            .get(var.key)
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let value = if value.is_empty() {
            var.fallback
        } else {
            value
        };
        let value = value.replace("<!--", "").replace("-->", "");
        rendered = rendered.replace(&format!("{{{{{}}}}}", var.key), &value);
    }
    rendered
}

pub fn normalize_body(text: &str) -> String {
    format!("{}\n", text.replace("\r\n", "\n").trim_end_matches('\n'))
}

pub fn stamp_content(body: &str) -> String {
    let normalized = normalize_body(body);
    format!(
        "{normalized}<!-- wt-managed {} -->\n",
        content_hash(&normalized)
    )
}

pub fn split_stamp(text: &str) -> (String, Option<String>) {
    let Some(without_final_nl) = text.strip_suffix('\n') else {
        return (text.to_owned(), None);
    };
    let Some(marker_start) = without_final_nl.rfind("<!-- wt-managed ") else {
        return (text.to_owned(), None);
    };
    let marker = &without_final_nl[marker_start..];
    let Some(hash) = marker
        .strip_prefix("<!-- wt-managed ")
        .and_then(|value| value.strip_suffix(" -->"))
    else {
        return (text.to_owned(), None);
    };
    if hash.len() != 12
        || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        || (marker_start > 0 && !without_final_nl[..marker_start].ends_with('\n'))
    {
        return (text.to_owned(), None);
    }
    let body = without_final_nl[..marker_start].to_owned();
    (body, Some(hash.to_owned()))
}

pub fn strip_rulesync_keys(markdown: &str) -> String {
    let Some(rest) = markdown.strip_prefix("---\n") else {
        return markdown.to_owned();
    };
    let Some((frontmatter, body)) = rest.split_once("\n---\n") else {
        return markdown.to_owned();
    };
    let mut out = Vec::new();
    let mut skipping = false;
    for line in frontmatter.lines() {
        if skipping {
            if line.starts_with(' ') || line.starts_with('\t') {
                continue;
            }
            skipping = false;
        }
        if line.starts_with("targets:") {
            skipping = true;
            continue;
        }
        out.push(line);
    }
    format!("---\n{}\n---\n{}", out.join("\n"), body)
}

const BEGIN_PREFIX: &str = "<!-- wt:instructions:begin ";
const END_MARKER: &str = "<!-- wt:instructions:end -->";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionsBlock {
    pub hash: String,
    pub body: String,
}

/// The number of managed blocks a file appears to carry. A stray begin or
/// end marker counts, so a file with a damaged block reports at least one and
/// is never treated as block-free (which would append a second block).
pub fn count_instructions_blocks(text: &str) -> usize {
    text.matches(BEGIN_PREFIX)
        .count()
        .max(text.matches(END_MARKER).count())
}

/// Byte offsets of the first well-formed block: the begin marker, the body,
/// and the end of the end marker.
struct BlockSpan<'a> {
    start: usize,
    hash: &'a str,
    body: std::ops::Range<usize>,
    end: usize,
}

fn block_span(text: &str) -> Option<BlockSpan<'_>> {
    let start = text.find(BEGIN_PREFIX)?;
    let after = &text[start + BEGIN_PREFIX.len()..];
    let marker_end = after.find("-->")?;
    let marker = &after[..marker_end];
    let hash = marker.split_whitespace().next()?;
    if hash.len() != 12 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let body_start = start + BEGIN_PREFIX.len() + marker_end + 3;
    let body_start = if text[body_start..].starts_with('\n') {
        body_start + 1
    } else {
        return None;
    };
    let body_end = text[body_start..].find(&format!("\n{END_MARKER}"))? + body_start;
    Some(BlockSpan {
        start,
        hash,
        body: body_start..body_end,
        end: body_end + 1 + END_MARKER.len(),
    })
}

pub fn extract_instructions_block(text: &str) -> Option<InstructionsBlock> {
    let span = block_span(text)?;
    Some(InstructionsBlock {
        hash: span.hash.to_owned(),
        body: text[span.body].to_owned(),
    })
}

/// Replace the managed block, or append one to a file that has none. Callers
/// must not splice into a file whose markers are damaged or duplicated
/// (`count_instructions_blocks` reports it); that would append a second block.
pub fn splice_instructions_block(file_text: &str, block_body: &str) -> String {
    let body = block_body.trim_end_matches('\n');
    let block = format!(
        "{BEGIN_PREFIX}{} (managed by `wt skills`; edits inside are overwritten) -->\n{body}\n{END_MARKER}",
        content_hash(body)
    );
    if let Some(span) = block_span(file_text) {
        return format!(
            "{}{}{}",
            &file_text[..span.start],
            block,
            &file_text[span.end..]
        );
    }
    if file_text.trim().is_empty() {
        return format!("{block}\n");
    }
    let separator = if file_text.ends_with("\n\n") {
        ""
    } else if file_text.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{file_text}{separator}{block}\n")
}

/// The file without its one well-formed managed block, keeping everything
/// outside the markers. None when there is no single intact block to remove.
pub fn remove_instructions_block(file_text: &str) -> Option<String> {
    if count_instructions_blocks(file_text) != 1 {
        return None;
    }
    let span = block_span(file_text)?;
    let before = file_text[..span.start].trim_end_matches('\n');
    let after = file_text[span.end..].trim_start_matches('\n');
    Some(match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => after.to_owned(),
        (false, true) => format!("{before}\n"),
        (false, false) => format!("{before}\n\n{after}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_the_block_keeps_surrounding_text() {
        let with = splice_instructions_block("mine\n", "RULES");
        assert_eq!(remove_instructions_block(&with).as_deref(), Some("mine\n"));
        let only = splice_instructions_block("", "RULES");
        assert_eq!(remove_instructions_block(&only).as_deref(), Some(""));
        let middle = format!("top\n\n{}\nbottom\n", only.trim_end());
        assert_eq!(
            remove_instructions_block(&middle).as_deref(),
            Some("top\n\nbottom\n")
        );
        assert_eq!(remove_instructions_block("mine\n"), None);
        let damaged = only.replace(END_MARKER, "");
        assert_eq!(remove_instructions_block(&damaged), None);
    }
    #[test]
    fn hash_stamp_and_render_are_stable() {
        let var = TemplateVar {
            key: "notes",
            prompt: "",
            fallback: "none",
        };
        let vars = [var];
        let answers = BTreeMap::from([("notes".into(), "hello -->".into())]);
        assert_eq!(
            render_template("{{notes}} {{unknown}}", &vars, &answers),
            "hello  {{unknown}}"
        );
        let stamped = stamp_content("a\r\nb\n\n");
        let (body, hash) = split_stamp(&stamped);
        assert_eq!(body, "a\nb\n");
        assert_eq!(hash.as_deref(), Some(content_hash(&body).as_str()));
    }

    #[test]
    fn instructions_replace_only_managed_region() {
        let one = splice_instructions_block("user text\n", "RULES");
        let two = splice_instructions_block(&one, "NEW");
        assert!(two.starts_with("user text\n\n"));
        assert_eq!(extract_instructions_block(&two).unwrap().body, "NEW");
        assert_eq!(count_instructions_blocks(&format!("{two}\n{two}")), 2);
    }

    #[test]
    fn splice_replaces_exactly_the_extracted_block() {
        // An end marker quoted inside the body must not cut the block short.
        let one = splice_instructions_block("a\n", "quoted <!-- wt:instructions:end --> inline");
        let two = splice_instructions_block(&format!("{one}tail\n"), "NEW");
        assert_eq!(extract_instructions_block(&two).unwrap().body, "NEW");
        assert!(
            two.ends_with("<!-- wt:instructions:end -->\ntail\n"),
            "{two}"
        );
        assert!(!two.contains("inline"));
    }

    #[test]
    fn damaged_markers_still_count_as_a_block() {
        let one = splice_instructions_block("", "RULES");
        let without_end = one.replace(END_MARKER, "");
        assert!(extract_instructions_block(&without_end).is_none());
        assert_eq!(count_instructions_blocks(&without_end), 1);
        assert_eq!(count_instructions_blocks("plain text"), 0);
    }
}
