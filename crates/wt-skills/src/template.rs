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

pub fn count_instructions_blocks(text: &str) -> usize {
    text.match_indices(BEGIN_PREFIX)
        .count()
        .min(text.matches(END_MARKER).count())
}

pub fn extract_instructions_block(text: &str) -> Option<InstructionsBlock> {
    let begin = text.find(BEGIN_PREFIX)?;
    let after = &text[begin + BEGIN_PREFIX.len()..];
    let marker_end = after.find("-->")?;
    let marker = &after[..marker_end];
    let hash = marker.split_whitespace().next()?;
    if hash.len() != 12 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let body_start = begin + BEGIN_PREFIX.len() + marker_end + 3;
    let body_start = if text[body_start..].starts_with('\n') {
        body_start + 1
    } else {
        return None;
    };
    let end = text[body_start..].find(&format!("\n{END_MARKER}"))? + body_start;
    Some(InstructionsBlock {
        hash: hash.to_owned(),
        body: text[body_start..end].to_owned(),
    })
}

pub fn splice_instructions_block(file_text: &str, block_body: &str) -> String {
    let body = block_body.trim_end_matches('\n');
    let block = format!(
        "{BEGIN_PREFIX}{} (managed by `wt skills`; edits inside are overwritten) -->\n{body}\n{END_MARKER}",
        content_hash(body)
    );
    if extract_instructions_block(file_text).is_some() {
        let start = file_text
            .find(BEGIN_PREFIX)
            .expect("extracted block has marker");
        let end = file_text[start..]
            .find(END_MARKER)
            .expect("extracted block has end")
            + start
            + END_MARKER.len();
        return format!("{}{}{}", &file_text[..start], block, &file_text[end..]);
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
