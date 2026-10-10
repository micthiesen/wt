#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use wt_config::{Config, LoadOptions};
use wt_github::{GithubClient, GithubOptions};
use wt_platform::lock::FileLock;
use wt_platform::process::ProcessRunner;
use wt_stack::{
    RestackOptions, RestackOutcome, StackConfig, StackEvent, StackService, StateConfig,
};
use wt_store::{RepositoryIdentity, Store};
use wt_vcs::{GitRepository, RepositoryConfig, StageConfig};

struct Fixture {
    root: tempfile::TempDir,
    main: PathBuf,
    worktrees: PathBuf,
    child: PathBuf,
    fake_gh: PathBuf,
    service: StackService,
}

impl Fixture {
    fn squash_parent() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let main = root.path().join("main clone");
        let worktrees = root.path().join("worktrees");
        let parent = worktrees.join("parent");
        let child = worktrees.join("child");
        let remote = root.path().join("origin.git");
        let home = root.path().join("home");
        fs::create_dir_all(&main)?;
        fs::create_dir_all(&worktrees)?;
        fs::create_dir_all(&home)?;
        git(
            &root.path().join("."),
            ["init", "--bare", remote.to_str().unwrap()],
        )?;
        git(&main, ["init", "-b", "main"])?;
        git(&main, ["config", "user.name", "Stack Fixture"])?;
        git(&main, ["config", "user.email", "stack@example.invalid"])?;
        fs::write(main.join("base.txt"), "base\n")?;
        git(&main, ["add", "base.txt"])?;
        git(&main, ["commit", "-m", "base"])?;
        let original_main = output(&main, ["rev-parse", "HEAD"])?;
        git(&main, ["remote", "add", "origin", remote.to_str().unwrap()])?;
        git(&main, ["push", "-u", "origin", "main"])?;
        git(
            &main,
            [
                "worktree",
                "add",
                "-b",
                "parent",
                parent.to_str().unwrap(),
                "main",
            ],
        )?;
        fs::write(parent.join("parent.txt"), "landed parent work\n")?;
        git(&parent, ["add", "parent.txt"])?;
        git(&parent, ["commit", "-m", "parent feature"])?;
        let parent_head = output(&parent, ["rev-parse", "HEAD"])?;
        git(&parent, ["push", "-u", "origin", "parent"])?;
        git(
            &main,
            [
                "worktree",
                "add",
                "-b",
                "child",
                child.to_str().unwrap(),
                "parent",
            ],
        )?;
        fs::write(child.join("child.txt"), "child work\n")?;
        git(&child, ["add", "child.txt"])?;
        git(&child, ["commit", "-m", "child feature"])?;
        git(&child, ["push", "-u", "origin", "child"])?;
        git(&main, ["merge", "--squash", "parent"])?;
        git(&main, ["commit", "-m", "squash parent into main"])?;
        git(&main, ["push", "origin", "main"])?;

        let state_path = root.path().join("state.sqlite");
        let identity = RepositoryIdentity::new("fixture", main.to_string_lossy());
        let mut store = Store::open(&state_path, identity.clone())?;
        store.set_slug_base("parent", Some(("main", Some(&original_main))))?;
        store.set_slug_base("child", Some(("parent", Some(&parent_head))))?;
        drop(store);

        fs::write(
            main.join(".wt.toml"),
            format!(
                "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\nstate_db = {:?}\ncache_db = {:?}\n[branch]\nprefix = \"feature/\"\nbase = \"main\"\n[stage]\nprefix = \"stage-\"\n",
                main.to_string_lossy(),
                worktrees.to_string_lossy(),
                state_path.to_string_lossy(),
                root.path().join("cache.sqlite").to_string_lossy()
            ),
        )?;
        let config = Config::load(&LoadOptions::new(&main, &home, BTreeMap::new()))?;
        let process = ProcessRunner::default();
        let repository = GitRepository::new(
            RepositoryConfig {
                main_clone: main.clone(),
                worktree_root: worktrees.clone(),
                trunk_branch: "main".into(),
                stage: StageConfig {
                    prefix: config.stage.prefix.clone(),
                    issue_id_pattern: config.branch.id_pattern.clone(),
                },
            },
            process.clone(),
        );
        let fake_gh = root.path().join("gh-fixture");
        fs::write(
            &fake_gh,
            r##"#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
if len(args) >= 3 and args[:2] == ['pr', 'view']:
    branch = args[2]
    if branch == 'parent':
        print(json.dumps({'number': 1, 'baseRefName': 'main', 'state': 'MERGED', 'isDraft': False, 'title': 'Parent', 'id': 'PR_parent', 'headRefOid': ''}))
        sys.exit(0)
    if branch == 'child':
        print(json.dumps({'number': 2, 'baseRefName': 'parent', 'state': 'OPEN', 'isDraft': False, 'title': 'Child', 'id': 'PR_child', 'headRefOid': ''}))
        sys.exit(0)
    sys.exit(1)
if len(args) >= 2 and args[:2] == ['pr', 'edit']:
    sys.exit(0)
sys.exit(1)
"##,
        )?;
        fs::set_permissions(&fake_gh, fs::Permissions::from_mode(0o755))?;
        let github = GithubClient::new(
            process.clone(),
            main.clone(),
            GithubOptions::from_config(&config, false),
        )
        .with_gh_program(fake_gh.clone());
        let service = StackService::new(
            StackConfig {
                main_clone: main.clone(),
                lock_dir: root.path().join("locks"),
                trunk_branch: "main".into(),
                fetch_options: Default::default(),
                state: StateConfig {
                    path: state_path,
                    identity,
                },
            },
            repository,
            process.clone(),
            github,
        );
        Ok(Self {
            root,
            main,
            worktrees,
            child,
            fake_gh,
            service,
        })
    }
}

fn output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("run fixture git")?;
    if !output.status.success() {
        anyhow::bail!("git failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("run fixture git")?;
    if !output.status.success() {
        anyhow::bail!("git failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

#[tokio::test]
async fn merged_parent_squash_rebases_child_from_recorded_anchor() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let result = fixture
        .service
        .restack("child", RestackOptions::default(), &cancel, &mut |event| {
            events.push(event)
        })
        .await?;
    assert_eq!(
        result,
        RestackOutcome::Complete {
            replayed: 1,
            total: 1
        }
    );
    assert!(fixture.child.join("parent.txt").exists());
    assert_eq!(
        fs::read_to_string(fixture.child.join("child.txt"))?,
        "child work\n"
    );
    let parent_sha = output(&fixture.worktrees.join("parent"), ["rev-parse", "HEAD"])?;
    assert!(
        !output(
            &fixture.child,
            ["merge-base", "--is-ancestor", &parent_sha, "HEAD"]
        )
        .is_ok()
    );
    let mut state = Store::open(
        fixture.root.path().join("state.sqlite"),
        RepositoryIdentity::new("fixture", fixture.main.to_string_lossy()),
    )?;
    let state = state.read_wt_state()?;
    assert_eq!(state["slugs"]["child"]["baseBranch"], "main");
    assert_eq!(
        state["slugs"]["child"]["baseSha"],
        output(&fixture.main, ["rev-parse", "origin/main"])?
    );
    assert!(events.iter().any(|event| matches!(event, StackEvent::Log(line) if line.contains("reparented child onto main"))));
    assert!(
        events.iter().any(
            |event| matches!(event, StackEvent::Log(line) if line.contains("retargeted PR #2"))
        )
    );
    drop(state);
    Ok(())
}

#[tokio::test]
async fn conflicting_replay_aborts_clean_and_retains_the_original_tip_backup() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    fs::write(fixture.child.join("base.txt"), "child edit\n")?;
    git(&fixture.child, ["add", "base.txt"])?;
    git(
        &fixture.child,
        ["commit", "-m", "child conflicts with main"],
    )?;
    git(&fixture.child, ["push", "origin", "child"])?;
    let old_head = output(&fixture.child, ["rev-parse", "HEAD"])?;
    fs::write(fixture.main.join("base.txt"), "main edit\n")?;
    git(&fixture.main, ["add", "base.txt"])?;
    git(&fixture.main, ["commit", "-m", "main conflicts with child"])?;
    git(&fixture.main, ["push", "origin", "main"])?;

    let mut events = Vec::new();
    let result = fixture
        .service
        .restack(
            "child",
            RestackOptions::default(),
            &CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .await?;
    let RestackOutcome::Conflict {
        branch,
        backup_ref,
        error,
    } = result
    else {
        anyhow::bail!("expected a content-conflict bail, got {result:?}");
    };
    assert_eq!(branch, "child");
    assert!(error.contains("base.txt"));
    assert_eq!(output(&fixture.child, ["rev-parse", "HEAD"])?, old_head);
    assert_eq!(
        output(
            &fixture.child,
            ["show-ref", "--verify", &format!("refs/heads/{backup_ref}")]
        )?
        .split_whitespace()
        .next(),
        Some(old_head.as_str())
    );
    assert!(output(&fixture.child, ["status", "--porcelain"])?.is_empty());
    for name in ["rebase-merge", "rebase-apply"] {
        let git_path = output(&fixture.child, ["rev-parse", "--git-path", name])?;
        let git_path = Path::new(git_path.trim());
        let git_path = if git_path.is_absolute() {
            git_path.to_path_buf()
        } else {
            fixture.child.join(git_path)
        };
        assert!(
            !git_path.exists(),
            "unfinished rebase state remains at {}",
            git_path.display()
        );
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StackEvent::Log(line) if line.contains("replay child")))
    );

    // Resolve the bailed rebase by hand, then rerun. The stored fork anchor
    // is stale after the manual rebase; live merge-base recovery must avoid
    // replaying the squashed parent a second time and sync the remote tip.
    let parent_sha = output(&fixture.worktrees.join("parent"), ["rev-parse", "HEAD"])?;
    let manual = Command::new("git")
        .args(["rebase", "--onto", "origin/main", &parent_sha, "child"])
        .current_dir(&fixture.child)
        .output()?;
    assert!(
        !manual.status.success(),
        "fixture should need manual conflict resolution"
    );
    fs::write(fixture.child.join("base.txt"), "main edit\nchild edit\n")?;
    git(&fixture.child, ["add", "base.txt"])?;
    let continued = Command::new("git")
        .args(["-c", "core.editor=true", "rebase", "--continue"])
        .current_dir(&fixture.child)
        .output()?;
    assert!(
        continued.status.success(),
        "manual rebase continuation failed: {}",
        String::from_utf8_lossy(&continued.stderr)
    );
    let rerun = fixture
        .service
        .restack(
            "child",
            RestackOptions::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await?;
    assert_eq!(
        rerun,
        RestackOutcome::Complete {
            replayed: 0,
            total: 1
        }
    );
    assert_eq!(
        output(&fixture.child, ["rev-parse", "HEAD"])?,
        output(&fixture.child, ["rev-parse", "origin/child"])?
    );
    Ok(())
}

#[tokio::test]
async fn missing_external_parent_and_no_pr_reconcile_to_trunk() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    git(
        &fixture.main,
        [
            "worktree",
            "remove",
            "--force",
            fixture.worktrees.join("parent").to_str().unwrap(),
        ],
    )?;
    git(&fixture.main, ["branch", "-D", "parent"])?;
    git(&fixture.main, ["push", "origin", "--delete", "parent"])?;
    fs::write(
        &fixture.fake_gh,
        "#!/usr/bin/env python3\nimport sys\nif sys.argv[1:3] == ['pr','edit']: sys.exit(0)\nsys.exit(1)\n",
    )?;
    let mut events = Vec::new();
    let result = fixture
        .service
        .restack(
            "child",
            RestackOptions::default(),
            &CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .await?;
    assert_eq!(
        result,
        RestackOutcome::Complete {
            replayed: 1,
            total: 1
        }
    );
    let mut store = Store::open(
        fixture.root.path().join("state.sqlite"),
        RepositoryIdentity::new("fixture", fixture.main.to_string_lossy()),
    )?;
    let state = store.read_wt_state()?;
    assert_eq!(state["slugs"]["child"]["baseBranch"], "main");
    assert!(events.iter().any(
        |event| matches!(event, StackEvent::Log(line) if line.contains("parent parent is gone"))
    ));
    Ok(())
}

#[tokio::test]
async fn dirty_tracked_checkout_is_refused_before_any_branch_rewrite() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    let old_head = output(&fixture.child, ["rev-parse", "HEAD"])?;
    fs::write(fixture.child.join("child.txt"), "uncommitted edit\n")?;
    let result = fixture
        .service
        .restack(
            "child",
            RestackOptions::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await?;
    assert!(
        matches!(result, RestackOutcome::Refused { error } if error.contains("uncommitted or untracked changes"))
    );
    assert_eq!(output(&fixture.child, ["rev-parse", "HEAD"])?, old_head);
    Ok(())
}

#[tokio::test]
async fn untracked_checkout_is_refused_without_changing_the_tip() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    let old_head = output(&fixture.child, ["rev-parse", "HEAD"])?;
    fs::write(fixture.child.join("untracked payload.txt"), "keep me\n")?;
    let result = fixture
        .service
        .restack(
            "child",
            RestackOptions::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await?;
    assert!(
        matches!(result, RestackOutcome::Refused { error } if error.contains("uncommitted or untracked changes"))
    );
    assert_eq!(output(&fixture.child, ["rev-parse", "HEAD"])?, old_head);
    assert_eq!(
        fs::read_to_string(fixture.child.join("untracked payload.txt"))?,
        "keep me\n"
    );
    Ok(())
}

#[tokio::test]
async fn unfinished_rebase_is_refused_without_touching_rebase_state() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    let old_head = output(&fixture.child, ["rev-parse", "HEAD"])?;
    let rebase_marker = output(&fixture.child, ["rev-parse", "--git-path", "rebase-merge"])?;
    let rebase_marker = Path::new(&rebase_marker);
    let rebase_marker = if rebase_marker.is_absolute() {
        rebase_marker.to_path_buf()
    } else {
        fixture.child.join(rebase_marker)
    };
    fs::create_dir_all(&rebase_marker)?;
    fs::write(rebase_marker.join("head-name"), "refs/heads/child\n")?;

    let result = fixture
        .service
        .restack(
            "child",
            RestackOptions::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await?;
    assert!(matches!(result, RestackOutcome::Refused { error } if error.contains("is mid-rebase")));
    assert_eq!(output(&fixture.child, ["rev-parse", "HEAD"])?, old_head);
    assert!(rebase_marker.join("head-name").exists());
    Ok(())
}

#[tokio::test]
async fn stale_force_with_lease_keeps_replay_backup_and_refuses_to_overwrite_remote() -> Result<()>
{
    stale_lease_fixture(false).await
}

#[tokio::test]
async fn concurrent_fetch_cannot_expand_the_force_push_lease() -> Result<()> {
    stale_lease_fixture(true).await
}

async fn stale_lease_fixture(refresh_tracking: bool) -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    let old_child = output(&fixture.child, ["rev-parse", "HEAD"])?;
    let competing_remote_tip = output(&fixture.main, ["rev-parse", "main"])?;
    let remote = fixture.root.path().join("origin.git");
    let hook = fixture.main.join(if refresh_tracking {
        ".git/hooks/post-rewrite"
    } else {
        ".git/hooks/pre-push"
    });
    let fetch = if refresh_tracking {
        // This runs after replay but before push. A background fetch can move
        // shared tracking refs without changing what restack actually reviewed.
        "subprocess.run([\"git\", \"fetch\", \"origin\", \"+refs/heads/child:refs/remotes/origin/child\"], check=True)\n".to_owned()
    } else {
        String::new()
    };
    fs::write(
        &hook,
        format!(
            "#!/usr/bin/env python3\nimport subprocess\nsubprocess.run([\"git\", \"--git-dir\", {remote:?}, \"update-ref\", \"refs/heads/child\", {competing_remote_tip:?}, {old_child:?}], check=True)\n{fetch}"
        ),
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    git(
        &fixture.child,
        [
            "config",
            "core.hooksPath",
            hook.parent().unwrap().to_str().unwrap(),
        ],
    )?;

    let result = fixture
        .service
        .restack(
            "child",
            RestackOptions::default(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await?;
    assert!(
        matches!(&result, RestackOutcome::Refused { error } if error.contains("force-with-lease") || error.contains("stale info") || error.contains("incorrect old value provided")),
        "stale lease should be refused, got {result:?}"
    );
    let replayed_tip = output(&fixture.child, ["rev-parse", "HEAD"])?;
    assert_ne!(
        replayed_tip, old_child,
        "the replayed branch should be recoverable"
    );
    assert_eq!(
        output(&fixture.child, ["rev-parse", "origin/child"])?,
        if refresh_tracking {
            competing_remote_tip.clone()
        } else {
            old_child.clone()
        },
        "the lease must retain its original expectation even if tracking refs refresh"
    );
    assert_eq!(
        output(
            &fixture.main,
            [
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/child"
            ]
        )?,
        competing_remote_tip,
        "the concurrently advanced remote ref must not be overwritten"
    );
    let refs = output(
        &fixture.child,
        [
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/backup/",
        ],
    )?;
    let backup = refs
        .lines()
        .find(|line| line.starts_with("backup/restack-"))
        .context("failed lease must retain its recovery backup")?;
    assert_eq!(
        output(&fixture.child, ["rev-parse", backup])?,
        old_child,
        "backup must retain the pre-replay tip"
    );
    Ok(())
}

#[tokio::test]
async fn backup_pruning_deletes_only_old_recognized_backup_refs() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    let old_tip = output(&fixture.child, ["rev-parse", "HEAD"])?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let old_ref = format!(
        "backup/restack-{}-feature/old-name",
        now_ms - 40 * 86_400_000
    );
    let recent_ref = format!("backup/restack-{}-feature/recent", now_ms + 60_000);
    let unknown_ref = "backup/not-a-wt-backup";
    for reference in [&old_ref, &recent_ref, unknown_ref] {
        let full_ref = format!("refs/heads/{reference}");
        git(&fixture.child, ["update-ref", &full_ref, &old_tip])?;
    }

    let result = fixture
        .service
        .prune_backups(30, &CancellationToken::new(), &mut |_| {})
        .await?;
    assert!(
        result.deleted.contains(&old_ref),
        "prune result: {result:?}; refs: {}",
        output(
            &fixture.main,
            [
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/backup/"
            ]
        )?
    );
    assert!(result.kept.contains(&recent_ref));
    assert!(result.kept.iter().any(|reference| reference == unknown_ref));
    assert!(
        output(
            &fixture.child,
            [
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{old_ref}")
            ]
        )
        .is_err()
    );
    assert_eq!(output(&fixture.child, ["rev-parse", &recent_ref])?, old_tip);
    assert_eq!(output(&fixture.child, ["rev-parse", unknown_ref])?, old_tip);
    Ok(())
}

#[tokio::test]
async fn stack_wait_cancels_while_a_member_lock_is_owned_elsewhere() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    let lock_dir = fixture.root.path().join("locks");
    let _held = FileLock::try_acquire(&lock_dir, "child", "fixture holder")
        .await?
        .unwrap();
    let cancellation = CancellationToken::new();
    let child = cancellation.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        child.cancel();
    });
    let result = fixture
        .service
        .restack(
            "child",
            RestackOptions::default(),
            &cancellation,
            &mut |_| {},
        )
        .await;
    cancel_task.await?;
    assert!(matches!(result, Err(wt_stack::StackError::Cancelled)));
    Ok(())
}

#[tokio::test]
async fn nonblocking_busy_probe_checks_the_whole_resolved_chain() -> Result<()> {
    let fixture = Fixture::squash_parent()?;
    let cancellation = CancellationToken::new();
    assert!(!fixture.service.is_busy("child", &cancellation).await?);

    let lock_dir = fixture.root.path().join("locks");
    let _held = FileLock::try_acquire(&lock_dir, "parent", "fixture holder")
        .await?
        .unwrap();
    assert!(fixture.service.is_busy("child", &cancellation).await?);
    Ok(())
}
