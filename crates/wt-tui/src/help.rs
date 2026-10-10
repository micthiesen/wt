//! The in-app keymap is shared by rendering and bounded keyboard scrolling.
//!
//! Lines are grouped into sections. A heading is a line with no leading
//! space; an entry starts with two spaces, a key column, then its meaning;
//! a blank line separates sections. Search keeps the heading of every
//! section that has a matching entry.
pub(crate) const LINES: &[&str] = &[
    "Navigation",
    "  j / k, arrows      Move cursor; at an edge, scroll the list",
    "  g / G, Home / End  First / last item",
    "  PgUp / PgDn        Move half a page",
    "  Space              Next row needing you (status, checks, review, failed action)",
    "  Tab                Fold / unfold the section under the cursor",
    "  Ctrl+D / Ctrl+U    Next / previous section",
    "  Ctrl+J / Ctrl+K    Scroll details",
    "",
    "Worktree",
    "  o                  Open in editor",
    "  p                  Open PR at the configured target",
    "  g p / l p          Open PR in GitHub / Linear",
    "  i / I              Open issue / primary tracker issue",
    "  #                  Set the tracker id (empty clears)",
    "  s                  Open deployed stage or dev server",
    "  y                  Copy picker: b s S d p n i I r, j/k, y/Enter, 1-9",
    "  y on folded        Copy section name (n), slugs (s), branches (b), list (l)",
    "  t / T              Edit title / regenerate AI title",
    "  V                  Show / hide this row's verify-after-merge steps",
    "  a                  Archive / restore",
    "  d                  Remove worktree (confirmation)",
    "",
    "Pull request",
    "  ! m                Toggle merge when ready",
    "  e                  Mark ready for review (confirmation)",
    "  E                  Ship: mark ready, request reviewer, arm merge",
    "  f                  Failed check logs",
    "  v                  Choose reviewers; Space toggles, v / Enter submits",
    "",
    "Sessions",
    "  !                  Action picker; stops a running action",
    "  ! <letter>         Run an action by its letter",
    "  ! u / ! g          Agent: update work status / continue work",
    "  ! d / ! s          Start or restart / stop the dev server",
    "  ! t                Rename worktree with AI",
    "  ! l                Dev server logs (l closes)",
    "  ! c                Custom prompt (paste keeps line breaks)",
    "  ;                  Sessions picker for the row",
    "  ; c / x / o        Jump to new Claude / Codex / OpenCode",
    "  ; d                Close highlighted session gracefully",
    "  ; x                Kill highlighted session (dead Claude: forget it)",
    "  ; 1-9              Attach a session row",
    "  Shift+Tab          Cycle primary harness",
    "  F12 / Shift+F12    Enter agent session / harness picker",
    "  F10 / Shift+F10    Enter / stop shell session",
    "  F11 / Shift+F11    Enter / stop diff session",
    "",
    "Organize",
    "  l                  Move to section (picker); l l confirms",
    "  l n                New section (Esc returns to the picker)",
    "  L                  Rename current section",
    "  b                  Record fork base (picker); b b confirms; never rebases",
    "  u                  Work status: t w r n h y v d, a ready + verify, x clears, m note",
    "  R                  Restack (rebase) the row or its stack; /restack on conflict",
    "  J / K              Move row, stack, or folded section",
    "",
    "Automations",
    "  A                  Pause / resume all automations",
    "  Ctrl+A             Pause / resume this row or its stack",
    "  Ctrl+Shift+A       Clear queued automations",
    "",
    "Manager",
    "  m                  Enter the manager session",
    "  M                  Manager palette; M M confirms",
    "  M d t o n a s      Digest, triage, merge order, nudge, audit, start next",
    "  M r / M m / M c    Ask about row / compact context / custom message",
    "",
    "Global",
    "  n / N              New worktree / based on the selected branch",
    "  Ctrl+N             New worktree on a chosen host",
    "  c                  Clean landed worktrees (all hosts)",
    "  h                  Removed-worktree history",
    "  r / Ctrl+R         Refresh / clear derived caches",
    "  , / . / /          wt source / main / dotfiles session",
    "  < / > / \\          Slot palette: g continue, m compact, z editor, c custom",
    "  O                  Open the main clone in the editor",
    "  P                  Performance overlay",
    "  ?                  Help; / searches",
    "  q / Ctrl+C         Quit",
    "",
    "Review requests",
    "  p / Enter          Open PR at the configured target",
    "  g p / l p          Open PR in GitHub / Linear",
    "  i                  Open linked issue",
    "  w                  Check out as a review worktree",
    "  d                  Dismiss until the PR changes",
    "  Tab                Fold / unfold requested reviews",
    "",
    "Removed worktrees (h)",
    "  Enter              Restore the worktree",
    "  p / i              Open recorded PR / issue",
    "  y                  Copy branch name",
    "  Ctrl+A             Pause / resume its automations",
    "  h / Esc            Back to worktrees",
    "",
    "Outputs",
    "  '                  Output picker; ' ' confirms",
    "  [ / ]              Previous / next stream",
    "  \"                  Attention / all activity",
    "  x                  Mark attention feed seen",
    "  Ctrl+E / Ctrl+Y    Scroll output down / up (Ctrl+Shift+J / K too)",
    "  Esc                Return the output pane to its default",
    "",
    "Performance (P)",
    "  j / k, PgUp / PgDn  Scroll",
    "  Ctrl+D/U, Ctrl+E/Y  Scroll by half a page / one line",
    "  i                  Send the snapshot to the wt session and enter it",
    "  c                  Toggle continuous sampling",
    "  r                  Sample now",
    "  P / Esc / q        Close",
    "",
    "Modals",
    "  key key            Re-press the opener to confirm (l l, ; ;, ! !)",
    "  Enter              Confirm highlighted",
    "  j / k, arrows      Move",
    "  1-9                Pick by number where offered",
    "  y / n              Confirm / cancel a confirmation",
    "  Esc / q / Ctrl+C   Cancel",
    "",
    "Mouse",
    "  Wheel              Scroll the pane under the pointer",
    "",
    "Selection",
    "  Mouse drag         Selects text; release copies it to the clipboard",
    "",
    "Sync notation",
    "  (↑N ↓M)            Ahead / behind vs the upstream (remote) branch",
    "  [↑N ↓M]            Ahead / behind vs the base branch",
    "",
    "New: prompt flags",
    "  --base <ref>       Branch off <ref> instead of the configured base",
    "  --attach           Attach to an existing branch for the id",
    "  --any              With --attach, match any author's branch",
];

/// A section heading, as opposed to an entry or a separator.
pub(crate) fn is_heading(line: &str) -> bool {
    !line.is_empty() && !line.starts_with(' ')
}

pub(crate) fn filtered_lines(query: &str) -> Vec<&'static str> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return LINES.to_vec();
    }
    let mut out = Vec::new();
    let mut heading = None;
    for line in LINES.iter().copied() {
        if line.is_empty() {
            continue;
        }
        if is_heading(line) {
            heading = Some(line);
            continue;
        }
        if line.to_lowercase().contains(&query) {
            if let Some(heading) = heading.take() {
                if !out.is_empty() {
                    out.push("");
                }
                out.push(heading);
            }
            out.push(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_keeps_the_heading_of_each_matching_section() {
        let lines = filtered_lines("graceful");
        assert_eq!(
            lines,
            vec![
                "Sessions",
                "  ; d                Close highlighted session gracefully"
            ]
        );
        assert!(filtered_lines("").len() == LINES.len());
        assert!(LINES.iter().all(|line| !line.contains('\u{2014}')));
        // Every entry separates its key column from its meaning with two
        // spaces; the overlay splits on that gap.
        for line in LINES.iter().filter(|line| line.starts_with("  ")) {
            assert!(
                line.trim().split_once("  ").is_some(),
                "entry without a key column: {line:?}"
            );
        }
    }
}
