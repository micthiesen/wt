use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessRunner};
use wt_vcs::{GitRepository, RepositoryConfig, RepositoryKind, StageConfig};

fn runner() -> ProcessRunner {
    ProcessRunner::new(NonZeroUsize::new(8).unwrap())
}

async fn git(runner: &ProcessRunner, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let mut spec = CommandSpec::new("git");
    spec.args = args.iter().map(std::ffi::OsString::from).collect();
    spec.cwd = Some(cwd.to_path_buf());
    spec.timeout = Duration::from_secs(20);
    spec.env = vec![
        ("GIT_CONFIG_NOSYSTEM".into(), Some("1".into())),
        ("GIT_CONFIG_GLOBAL".into(), Some("/dev/null".into())),
        ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
        ("GIT_AUTHOR_NAME".into(), Some("wt test".into())),
        (
            "GIT_AUTHOR_EMAIL".into(),
            Some("wt-test@example.invalid".into()),
        ),
        ("GIT_COMMITTER_NAME".into(), Some("wt test".into())),
        (
            "GIT_COMMITTER_EMAIL".into(),
            Some("wt-test@example.invalid".into()),
        ),
    ];
    runner
        .run(spec, &CancellationToken::new())
        .await
        .unwrap()
        .checked("git")
        .unwrap()
        .stdout
}

async fn fixture() -> (TempDir, ProcessRunner, PathBuf, PathBuf) {
    let scratch = tempfile::tempdir().unwrap();
    let base = scratch.path().join("repo with spaces");
    let main = base.join("main clone");
    let root = base.join("worktree root");
    tokio::fs::create_dir_all(&main).await.unwrap();
    tokio::fs::create_dir_all(&root).await.unwrap();
    let runner = runner();
    git(&runner, &main, &["init", "-b", "main"]).await;
    git(&runner, &main, &["config", "user.name", "wt test"]).await;
    git(
        &runner,
        &main,
        &["config", "user.email", "wt-test@example.invalid"],
    )
    .await;
    tokio::fs::write(main.join("tracked.txt"), "base\n")
        .await
        .unwrap();
    git(&runner, &main, &["add", "tracked.txt"]).await;
    git(&runner, &main, &["commit", "-m", "initial"]).await;
    (scratch, runner, main, root)
}

fn repository(main: PathBuf, root: PathBuf, runner: ProcessRunner) -> GitRepository {
    GitRepository::new(
        RepositoryConfig {
            main_clone: main,
            worktree_root: root,
            trunk_branch: "origin/main".into(),
            stage: StageConfig {
                prefix: "stage-".into(),
                issue_id_pattern: r"([A-Z]+-\d+)".into(),
            },
        },
        runner,
    )
}

#[tokio::test]
async fn inventory_keeps_linked_spaces_detached_and_rift_clones_distinct() {
    let (_scratch, runner, main, root) = fixture().await;
    let parent = root.join("feature parent");
    let child = root.join("feature child");
    let detached = root.join("detached checkout");
    git(
        &runner,
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "team/ENG-42-parent",
            parent.to_str().unwrap(),
        ],
    )
    .await;
    git(
        &runner,
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "team/ENG-43-child",
            child.to_str().unwrap(),
            "main",
        ],
    )
    .await;
    git(
        &runner,
        &main,
        &[
            "worktree",
            "add",
            "--detach",
            detached.to_str().unwrap(),
            "HEAD",
        ],
    )
    .await;
    let rift = root.join("rift independent");
    git(
        &runner,
        &main,
        &["clone", main.to_str().unwrap(), rift.to_str().unwrap()],
    )
    .await;
    tokio::fs::write(rift.join(".rift"), "rift marker\n")
        .await
        .unwrap();
    tokio::fs::write(main.join(".git/info/exclude"), ".wt.toml\n")
        .await
        .unwrap();
    tokio::fs::write(parent.join(".wt.toml"), "ignored config\n")
        .await
        .unwrap();

    let service = repository(main.clone(), root.clone(), runner.clone());
    let cancel = CancellationToken::new();
    let inventory = service.inventory(&cancel).await.unwrap();
    assert_eq!(inventory.len(), 5);
    let main_canonical = tokio::fs::canonicalize(&main).await.unwrap();
    let parent_canonical = tokio::fs::canonicalize(&parent).await.unwrap();
    let rift_canonical = tokio::fs::canonicalize(&rift).await.unwrap();
    assert!(
        inventory
            .iter()
            .any(|record| record.is_main && record.target.path == main_canonical.to_string_lossy()),
        "{inventory:#?}"
    );
    let linked = inventory
        .iter()
        .find(|record| record.target.branch == "team/ENG-42-parent")
        .unwrap();
    assert_eq!(linked.target.path, parent_canonical.to_string_lossy());
    assert_eq!(
        linked.target.stage,
        wt_core::stage_name("feature parent", "stage-", None)
    );
    assert!(
        linked
            .git_dir
            .as_ref()
            .unwrap()
            .ends_with(".git/worktrees/feature-parent")
    );
    assert!(linked.common_dir.as_ref().unwrap().ends_with(".git"));
    assert!(
        inventory
            .iter()
            .any(|record| record.target.branch.is_empty() && record.detached)
    );
    assert!(
        inventory
            .iter()
            .any(|record| record.kind == RepositoryKind::RiftClone
                && record.target.path == rift_canonical.to_string_lossy())
    );

    tokio::fs::write(parent.join("tracked.txt"), "base\nchanged\n")
        .await
        .unwrap();
    tokio::fs::write(parent.join("untracked file.txt"), "one\ntwo\n")
        .await
        .unwrap();
    let rows = service.inventory_status(&cancel).await.unwrap();
    let parent_row = rows
        .iter()
        .find(|row| row.worktree.target.branch == "team/ENG-42-parent")
        .unwrap();
    let status = parent_row.status.as_ref().unwrap();
    assert_eq!(status.tracked_changes, 1);
    assert_eq!(status.untracked_files, 1);
    assert!(status.dirty);
    assert_eq!(status.branch.as_deref(), Some("team/ENG-42-parent"));
    let diff = service
        .diff_stats(&parent, "main", &cancel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(diff.untracked_files, 1);
    assert_eq!(diff.untracked_lines, 2);
    assert!(diff.insertions >= 1);

    tokio::fs::remove_dir_all(&detached).await.unwrap();
    let rows = service.inventory_status(&cancel).await.unwrap();
    let missing = rows
        .iter()
        .find(|row| row.worktree.detached)
        .expect("stale worktree remains visible until Git prunes it");
    assert!(missing.status.is_none());
    assert!(
        missing
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Git status"))
    );
}
