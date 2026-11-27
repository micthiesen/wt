//! Whether a commit is a snapshot of a base branch rather than work of its
//! own. A fork anchor can lag behind a HEAD that only advanced along the base
//! (a fresh worktree created on a newer base tip, or re-anchored to an older
//! merge), so counting `anchor..HEAD` would call base commits its own work.
//! A commit on the base's first-parent line is one the base itself sat at; a
//! branch merged into the base is the second parent of its merge instead.

/// The longest first-parent walk taken. A longer one answers "not on the
/// line", leaving the caller's other landing evidence in charge.
pub const MAINLINE_WALK: usize = 2000;

/// `git rev-list` arguments whose output [`on_first_parent_line`] reads:
/// the base's first-parent commits newer than `head`, each with its parents.
pub fn mainline_args(base: &str, head: &str) -> Vec<String> {
    vec![
        "rev-list".into(),
        "--first-parent".into(),
        "--parents".into(),
        "-n".into(),
        MAINLINE_WALK.to_string(),
        base.into(),
        format!("^{head}"),
    ]
}

/// Read [`mainline_args`] output, given that `head` is already known to be an
/// ancestor of the base. Empty output means the base is at `head`; otherwise
/// the oldest listed commit must have `head` as its first parent.
pub fn on_first_parent_line(rev_list_parents: &str, head: &str) -> bool {
    let lines: Vec<&str> = rev_list_parents.lines().collect();
    match lines.last() {
        None => true,
        Some(_) if lines.len() >= MAINLINE_WALK => false,
        Some(oldest) => oldest.split_whitespace().nth(1) == Some(head),
    }
}

/// `git log` arguments whose output [`brought_in`] reads: the base's
/// first-parent commits that hold `head`, newest first, each as its
/// committer time and its parents.
pub fn brought_in_args(base: &str, head: &str) -> Vec<String> {
    vec![
        "log".into(),
        "--first-parent".into(),
        "--ancestry-path".into(),
        "--format=%ct %P".into(),
        "-n".into(),
        MAINLINE_WALK.to_string(),
        format!("{head}..{base}"),
    ]
}

/// What brought a contained `head` into its base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BroughtIn {
    /// A base commit, by its committer time in epoch seconds: the merge.
    At(u64),
    /// The base moved to `head` itself; its own commit time says when.
    Head,
}

/// Read [`brought_in_args`] output, given that `head` is an ancestor of the
/// base. The oldest listed commit is the first the base's own line made
/// with `head` in it: a merge, unless `head` is its first parent, in which
/// case the base sat at `head`. `None` for a walk cut at the limit.
pub fn brought_in(log: &str, head: &str) -> Option<BroughtIn> {
    let lines: Vec<&str> = log.lines().filter(|line| !line.is_empty()).collect();
    let Some(oldest) = lines.last() else {
        return Some(BroughtIn::Head);
    };
    if lines.len() >= MAINLINE_WALK {
        return None;
    }
    let mut fields = oldest.split_whitespace();
    let at = fields.next()?.parse().ok()?;
    Some(if fields.next() == Some(head) {
        BroughtIn::Head
    } else {
        BroughtIn::At(at)
    })
}

/// `git reflog` arguments whose output [`committed_on_branch`] reads: each
/// update of the branch as `<sha> <subject>`.
pub fn branch_reflog_args(branch: &str) -> Vec<String> {
    vec![
        "reflog".into(),
        "show".into(),
        "--format=%H %gs".into(),
        format!("refs/heads/{branch}"),
        "--".into(),
    ]
}

/// Whether the branch itself produced `head`: an update that wrote it was a
/// commit, a rebase or a cherry-pick. A fast-forward, reset, pull or branch
/// creation only moved the branch to a commit made elsewhere, and so did a
/// rebase that replayed nothing.
pub fn committed_on_branch(reflog: &str, head: &str) -> bool {
    reflog.lines().any(|line| {
        line.split_once(' ').is_some_and(|(sha, subject)| {
            sha == head
                && !rebase_replayed_nothing(sha, subject)
                && ["commit", "rebase", "cherry-pick", "revert", "am"]
                    .iter()
                    .any(|kind| {
                        subject
                            .strip_prefix(kind)
                            .is_some_and(|rest| rest.starts_with([':', ' ']))
                    })
        })
    })
}

/// A rebase that finished on the very commit it rebased onto: the branch had
/// nothing to replay (it was empty, or all of it was already upstream), so
/// the rebase only moved it to a base commit. Git writes the target as
/// `onto <sha>`, abbreviated or full depending on how it was named.
fn rebase_replayed_nothing(sha: &str, subject: &str) -> bool {
    if !subject.starts_with("rebase") {
        return false;
    }
    subject
        .rsplit_once(" onto ")
        .map(|(_, onto)| onto.trim())
        .is_some_and(|onto| !onto.is_empty() && (sha.starts_with(onto) || onto.starts_with(sha)))
}

/// Landing by containment: `head` is in the base and is either off the
/// base's own line (it was merged in) or a commit this branch made itself
/// (fast-forwarded in). A base snapshot the branch only moved to is not.
pub fn contained_head_landed(mainline: &str, reflog: &str, head: &str) -> bool {
    !on_first_parent_line(mainline, head) || committed_on_branch(reflog, head)
}

/// `git cherry <base> <head>` arguments: every commit of `head` the base
/// does not contain, marked `-` when the base holds a commit with the same
/// patch and `+` when it does not.
pub fn cherry_args(base: &str, head: &str) -> Vec<String> {
    vec!["cherry".into(), base.into(), head.into()]
}

/// The commits `git cherry` found in no form on the other side.
pub fn cherry_unmatched(cherry: &str) -> u32 {
    let unmatched = cherry
        .lines()
        .filter(|line| !line.starts_with("- "))
        .count();
    u32::try_from(unmatched).unwrap_or(u32::MAX)
}

/// Whether the branch has commits beyond the base and the base holds the
/// patch of every one: it was rebased after its work merged. A squash merge
/// never reads so, because a squashed patch matches none of its commits.
pub fn cherry_all_in_base(cherry: &str) -> bool {
    cherry.lines().next().is_some() && cherry_unmatched(cherry) == 0
}

/// What one Git command of [`base_holds_everything`] answered.
pub struct GitRead {
    pub code: Option<i32>,
    pub stdout: String,
}

/// What a branch has that its base does not, as far as Git can prove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeyondBase {
    /// The base holds all of it; so many commits were compared by patch.
    Nothing(u32),
    /// So many commits match no patch in the base.
    Commits(u32),
    /// A read failed, or patches match but the content may still differ.
    Unknown,
}

/// Whether `base` holds everything `head` has beyond it (and beyond
/// `pushed`, when the range is only what was never pushed).
///
/// `git cherry` alone is not that proof: it skips merge commits, whose
/// resolution is content of their own, and it matches a patch the base
/// applied and later reverted. So the range must also hold no merge commit,
/// and merging `head` into `base` must change nothing (Git 2.38+).
///
/// `run` runs one Git command in the checkout and gives `None` when it
/// could not run or its output was cut.
pub async fn base_holds_everything<F, Fut>(
    base: &str,
    head: &str,
    pushed: Option<&str>,
    run: F,
) -> BeyondBase
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Option<GitRead>>,
{
    let ok = |read: Option<GitRead>| {
        read.filter(|read| read.code == Some(0))
            .map(|read| read.stdout)
    };
    let mut cherry = cherry_args(base, head);
    cherry.extend(pushed.map(str::to_owned));
    let Some(cherry) = ok(run(cherry).await) else {
        return BeyondBase::Unknown;
    };
    let unmatched = cherry_unmatched(&cherry);
    if unmatched > 0 {
        return BeyondBase::Commits(unmatched);
    }
    let mut merges: Vec<String> = ["rev-list", "--merges", "--max-count=1", head, "--not", base]
        .map(str::to_owned)
        .into();
    merges.extend(pushed.map(str::to_owned));
    if ok(run(merges).await).is_none_or(|merges| !merges.trim().is_empty()) {
        return BeyondBase::Unknown;
    }
    let merged = ok(run(["merge-tree", "--write-tree", base, head]
        .map(str::to_owned)
        .into())
    .await);
    let base_tree = ok(run(vec![
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        "--end-of-options".into(),
        format!("{base}^{{tree}}"),
    ])
    .await);
    let merged_tree = merged
        .as_deref()
        .and_then(|merged| merged.lines().next())
        .map(str::trim)
        .filter(|tree| !tree.is_empty());
    match (merged_tree, base_tree) {
        (Some(merged), Some(base)) if merged == base.trim() => {
            BeyondBase::Nothing(u32::try_from(cherry.lines().count()).unwrap_or(u32::MAX))
        }
        _ => BeyondBase::Unknown,
    }
}

/// The most merge commits [`published_head_holds_everything`] re-creates.
const REPRODUCED_MERGES: usize = 8;

/// Whether `head` holds nothing that `published` and `base` lack, where
/// `published` is a commit known to exist elsewhere (the head a pull request
/// merged from, or the branch's own origin ref).
///
/// This is the proof for a branch that moved after it was published: rebased
/// onto a newer base, or with the base merged in, and never pushed again. It
/// compares with the published head itself, so it holds for a squash merge
/// and when the base has since changed the same files, where
/// [`base_holds_everything`] cannot answer.
///
/// Every commit `head` has beyond `base` and `published` must be either a
/// copy, by patch, of a commit `published` has, or a merge Git reproduces
/// exactly from its two parents, which therefore carries no resolution of
/// its own. With no such commit at all, `head` must be off the base: it is
/// then history of `published`. A `head` the base contains is left to the
/// containment proof, which knows a base snapshot from landed work.
///
/// `run` runs one Git command in the checkout and gives `None` when it
/// could not run or its output was cut.
pub async fn published_head_holds_everything<F, Fut>(
    published: &str,
    head: &str,
    base: &str,
    run: F,
) -> BeyondBase
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Option<GitRead>>,
{
    let ok = |read: Option<GitRead>| {
        read.filter(|read| read.code == Some(0))
            .map(|read| read.stdout)
    };
    let args = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    // An object this store does not have (pushed from another clone and
    // never fetched) proves nothing.
    let commit = format!("{published}^{{commit}}");
    if ok(run(args(&["cat-file", "-e", "--end-of-options", &commit])).await).is_none() {
        return BeyondBase::Unknown;
    }
    let Some(cherry) = ok(run(args(&["cherry", published, head, base])).await) else {
        return BeyondBase::Unknown;
    };
    let unmatched = cherry_unmatched(&cherry);
    if unmatched > 0 {
        return BeyondBase::Commits(unmatched);
    }
    let copies = cherry.lines().count();
    let limit = (REPRODUCED_MERGES + 1).to_string();
    let Some(merges) = ok(run(args(&[
        "rev-list",
        "--merges",
        "--parents",
        "-n",
        &limit,
        head,
        "--not",
        base,
        published,
    ]))
    .await) else {
        return BeyondBase::Unknown;
    };
    let merges: Vec<&str> = merges.lines().filter(|line| !line.is_empty()).collect();
    if merges.len() > REPRODUCED_MERGES {
        return BeyondBase::Unknown;
    }
    for merge in &merges {
        let fields: Vec<&str> = merge.split_whitespace().collect();
        let [merge, ours, theirs] = fields[..] else {
            return BeyondBase::Unknown;
        };
        let reproduced = ok(run(args(&["merge-tree", "--write-tree", ours, theirs])).await);
        let tree = format!("{merge}^{{tree}}");
        let recorded = ok(run(args(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &tree,
        ]))
        .await);
        let reproduced = reproduced
            .as_deref()
            .and_then(|tree| tree.lines().next())
            .map(str::trim)
            .filter(|tree| !tree.is_empty());
        match (reproduced, recorded.as_deref().map(str::trim)) {
            (Some(reproduced), Some(recorded)) if reproduced == recorded => {}
            _ => return BeyondBase::Unknown,
        }
    }
    if copies == 0 && merges.is_empty() {
        // Nothing beyond the two: `head` is history of one of them.
        match run(args(&["merge-base", "--is-ancestor", head, base]))
            .await
            .and_then(|read| read.code)
        {
            Some(1) => {}
            _ => return BeyondBase::Unknown,
        }
    }
    BeyondBase::Nothing(u32::try_from(copies).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_brought_a_branch_in_is_the_oldest_base_commit_holding_it() {
        // Newest first: two later merges, then the merge of `head`.
        let log = "300 m2 x\n200 m1 y\n100 base head\n";
        assert_eq!(brought_in(log, "head"), Some(BroughtIn::At(100)));
        // The base fast-forwarded to `head`: its own commit says when.
        assert_eq!(brought_in("200 head\n", "head"), Some(BroughtIn::Head));
        assert_eq!(brought_in("", "head"), Some(BroughtIn::Head));
        // A walk cut at the limit did not reach the commit.
        let long = "100 a b\n".repeat(MAINLINE_WALK);
        assert_eq!(brought_in(&long, "head"), None);
        assert_eq!(brought_in("garbage\n", "head"), None);
    }

    #[test]
    fn only_a_branch_whose_every_commit_is_matched_reads_as_in_the_base() {
        assert!(cherry_all_in_base("- a\n- b\n"));
        assert!(!cherry_all_in_base(""));
        assert!(!cherry_all_in_base("- a\n+ b\n"));
        assert_eq!(cherry_unmatched("- a\n+ b\n+ c\n"), 2);
        // An unreadable line is never a match.
        assert_eq!(cherry_unmatched("garbage\n"), 1);
    }

    #[test]
    fn only_a_branch_s_own_commit_counts_as_its_work() {
        let reflog = "h3 merge origin/main: Fast-forward\nh2 commit: own work\nh1 branch: Created from origin/main\n";
        assert!(committed_on_branch(reflog, "h2"));
        assert!(!committed_on_branch(reflog, "h3"));
        assert!(!committed_on_branch(reflog, "h1"));
        assert!(committed_on_branch(
            "h4 rebase (finish): refs/heads/x onto b\n",
            "h4"
        ));
        assert!(committed_on_branch("h5 commit (amend): fix\n", "h5"));
        assert!(committed_on_branch(
            "h6 rebase (finish): returning to refs/heads/x\n",
            "h6"
        ));
        // A snapshot the branch fast-forwarded to is not landed work; its own
        // fast-forwarded commit and a merged-in commit are.
        assert!(!contained_head_landed("", reflog, "h3"));
        assert!(contained_head_landed("", reflog, "h2"));
        assert!(contained_head_landed("m b h9\n", "", "h9"));
    }

    /// An empty branch rebased onto trunk sits on a trunk commit it never
    /// made; calling that landed let cleanup and branch deletion act on a
    /// worktree whose agent had not committed yet.
    #[test]
    fn a_rebase_that_replayed_nothing_is_a_move_not_own_work() {
        let onto = "affe40e1359e0c8f6a1d2b3c4d5e6f708192a3b4";
        let full = format!(
            "{onto} rebase (finish): refs/heads/feature onto {onto}\n{onto} branch: Created from origin/main\n"
        );
        assert!(!committed_on_branch(&full, onto));
        assert!(!contained_head_landed("", &full, onto));
        let abbreviated = format!("{onto} rebase (finish): refs/heads/feature onto affe40e135\n");
        assert!(!committed_on_branch(&abbreviated, onto));
        let short_head = "affe40e135 rebase (finish): refs/heads/feature onto affe40e1359e0c8f\n";
        assert!(!committed_on_branch(short_head, "affe40e135"));

        // Replayed commits end on a new hash, which is the branch's own.
        let real = format!(
            "b00c1e5d7f9a1b3c5d7e9f0a2b4c6d8e0f1a3b5c rebase (finish): refs/heads/feature onto {onto}\n"
        );
        assert!(committed_on_branch(
            &real,
            "b00c1e5d7f9a1b3c5d7e9f0a2b4c6d8e0f1a3b5c"
        ));
        // A later commit on that same base commit is still own work.
        let then_committed = format!("c0ffee commit: work\n{full}");
        assert!(committed_on_branch(&then_committed, "c0ffee"));
        // Merged in rather than fast-forwarded: off the base's own line.
        assert!(contained_head_landed("m b h9\n", &full, "h9"));
    }

    #[test]
    fn a_base_snapshot_is_on_the_line_and_a_merged_branch_is_not() {
        // The base at HEAD exactly: nothing newer on its line.
        assert!(on_first_parent_line("", "h"));
        // The base moved on from HEAD: c's first parent is HEAD.
        assert!(on_first_parent_line("d c\nc h\n", "h"));
        // HEAD was merged in: the merge's first parent is the base, HEAD is
        // only its second parent.
        assert!(!on_first_parent_line("m b h\n", "h"));
        // A walk that hit the bound proves nothing.
        let long = "x y\n".repeat(MAINLINE_WALK);
        assert!(!on_first_parent_line(&long, "y"));
    }
    /// A scratch repository for the published-head proof: `main` with one
    /// file, and a `work` branch one commit ahead that was "published" at
    /// the returned head.
    struct Repo(tempfile::TempDir);

    impl Repo {
        fn new() -> (Self, String) {
            let repo = Self(tempfile::tempdir().unwrap());
            repo.git(&["init", "-q", "-b", "main"]);
            repo.write("shared.txt", "one\ntwo\nthree\n");
            repo.commit("base");
            repo.git(&["checkout", "-q", "-b", "work"]);
            repo.write("work.txt", "work\n");
            repo.commit("work");
            let published = repo.head();
            (repo, published)
        }

        fn git(&self, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .current_dir(self.0.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        fn write(&self, name: &str, text: &str) {
            std::fs::write(self.0.path().join(name), text).unwrap();
        }

        fn commit(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
        }

        fn head(&self) -> String {
            self.git(&["rev-parse", "HEAD"])
        }

        /// A commit on `main` changing `name`, leaving `work` checked out.
        fn advance_main(&self, name: &str, text: &str) {
            self.git(&["checkout", "-q", "main"]);
            self.write(name, text);
            self.commit("main moves");
            self.git(&["checkout", "-q", "work"]);
        }

        async fn run(&self, args: Vec<String>) -> Option<GitRead> {
            let output = std::process::Command::new("git")
                .current_dir(self.0.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(&args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .ok()?;
            Some(GitRead {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            })
        }

        async fn published(&self, published: &str) -> BeyondBase {
            published_head_holds_everything(published, "HEAD", "main", |args| self.run(args)).await
        }
    }

    /// The pull request merged from the pushed head while the checkout had
    /// been rebased onto a newer base and not pushed, and the base later
    /// changed the same file: the row stayed "not landed" with unpushed
    /// commits, because merging it into the base no longer changed nothing.
    #[tokio::test]
    async fn a_copy_of_the_published_head_is_held_even_after_the_base_moved_on() {
        let (repo, published) = Repo::new();
        repo.advance_main("other.txt", "other\n");
        repo.git(&["rebase", "-q", "main"]);
        assert_ne!(repo.head(), published);
        // The merge, then a later change to the file the work added.
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge work", &published]);
        repo.write("work.txt", "work, edited on main\n");
        repo.commit("main edits the work");
        repo.git(&["checkout", "-q", "work"]);

        assert_eq!(repo.published(&published).await, BeyondBase::Nothing(1));
        let by_base = base_holds_everything("main", "HEAD", None, |args| repo.run(args)).await;
        assert_eq!(by_base, BeyondBase::Unknown);
    }

    #[tokio::test]
    async fn a_squash_merged_branch_rebased_afterwards_is_held() {
        let (repo, published) = Repo::new();
        repo.write("more.txt", "more\n");
        repo.commit("more work");
        let published_two = repo.head();
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["merge", "-q", "--squash", &published_two]);
        repo.commit("squash of work");
        repo.write("other.txt", "other\n");
        repo.commit("main moves");
        repo.git(&["checkout", "-q", "work"]);
        // Replayed onto the commit before the squash, as a rebase that ran
        // while the pull request sat in the merge queue would.
        repo.git(&["rebase", "-q", "--onto", "main~2", &published, "work"]);
        assert_eq!(repo.published(&published_two).await, BeyondBase::Nothing(1));
        // The first commit is history of the published head too.
        repo.git(&["reset", "-q", "--hard", &published]);
        assert_eq!(repo.published(&published_two).await, BeyondBase::Nothing(0));
    }

    #[tokio::test]
    async fn work_the_published_head_never_had_counts() {
        let (repo, published) = Repo::new();
        repo.advance_main("other.txt", "other\n");
        repo.git(&["rebase", "-q", "main"]);
        repo.write("later.txt", "later\n");
        repo.commit("a follow-up nobody pushed");
        assert_eq!(repo.published(&published).await, BeyondBase::Commits(1));
        // A revert of the published work is work of its own.
        repo.git(&["reset", "-q", "--hard", &published]);
        repo.git(&["revert", "--no-edit", "HEAD"]);
        assert_eq!(repo.published(&published).await, BeyondBase::Commits(1));
    }

    #[tokio::test]
    async fn a_merge_of_the_base_counts_only_when_it_holds_a_resolution() {
        let (repo, published) = Repo::new();
        repo.advance_main("other.txt", "other\n");
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge main", "main"]);
        assert_eq!(repo.published(&published).await, BeyondBase::Nothing(0));
        // The same merge with a change Git would not have made.
        repo.write("other.txt", "other, resolved by hand\n");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "--amend", "--no-edit"]);
        assert_eq!(repo.published(&published).await, BeyondBase::Unknown);
    }

    #[tokio::test]
    async fn a_head_the_base_contains_and_an_unknown_object_prove_nothing() {
        let (repo, published) = Repo::new();
        // A snapshot of the base is the containment proof's to judge.
        repo.git(&["reset", "-q", "--hard", "main"]);
        assert_eq!(repo.published(&published).await, BeyondBase::Unknown);
        repo.git(&["reset", "-q", "--hard", &published]);
        let absent = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(repo.published(absent).await, BeyondBase::Unknown);
    }
}
