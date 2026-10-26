use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_store::GuideRecord;

use wt_app_core::context::AppContext;
use wt_ops::commands::resolve::resolve_worktree;

#[derive(Debug, Clone, Args, Default)]
pub struct GuideArgs {
    /// `[<slug>] [<url>]`: a lone argument that is a URL records it for the
    /// current worktree.
    #[arg(value_name = "SLUG_OR_URL", num_args = 0..=2)]
    pub positionals: Vec<String>,
    #[arg(long, conflicts_with = "json")]
    pub clear: bool,
    #[arg(long)]
    pub json: bool,
}

pub async fn run(ctx: &AppContext, args: &GuideArgs) -> Result<i32> {
    let (target, url) = match args.positionals.as_slice() {
        [] => (None, None),
        [one] if is_url(one) => (None, Some(one.as_str())),
        [one] => (Some(one.as_str()), None),
        [slug, url] => (Some(slug.as_str()), Some(url.as_str())),
        _ => unreachable!("clap limits positionals to two"),
    };
    let enabled = ctx.config.guides.enabled;
    if (url.is_some() || args.clear) && !enabled {
        eprintln!(
            "change guides are off, so nothing was recorded; set `[guides] enabled = true` in wt's config to use them"
        );
        return Ok(2);
    }
    if let Some(url) = url
        && (!is_url(url) || url.chars().any(|c| c.is_whitespace() || c.is_control()))
    {
        eprintln!("not a guide URL: {url} (expected one http(s) URL)");
        return Ok(2);
    }
    let record = match resolve_worktree(ctx, target).await {
        Ok(record) if !record.is_main => record,
        Ok(_) => {
            eprintln!("not inside a worktree; pass a slug");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    let slug = record.target.slug().to_owned();
    let head = record.head_sha.clone();
    if url.is_some() || args.clear {
        let guide = url.map(|url| GuideRecord {
            url: url.to_owned(),
            sha: head.clone(),
            at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default(),
        });
        let db_slug = slug.clone();
        let changed = ctx
            .database
            .call(move |store| Ok(store.set_slug_guide(&db_slug, guide.as_ref())?))
            .await?;
        match url {
            Some(url) => println!(
                "✓ {slug} guide{} for {}: {url}",
                if changed { "" } else { " (already recorded)" },
                short(head.as_deref())
            ),
            None => println!(
                "✓ {slug} guide {}",
                if changed { "cleared" } else { "was not set" }
            ),
        }
        return Ok(0);
    }
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let guide = state
        .get("slugs")
        .and_then(|slugs| slugs.get(&slug))
        .and_then(|entry| entry.get("guide"))
        .and_then(|value| serde_json::from_value::<GuideRecord>(value.clone()).ok());
    let current = guide
        .as_ref()
        .map(|guide| guide.sha.is_some() && guide.sha == head);
    if args.json {
        println!(
            "{}",
            json!({
                "enabled": enabled,
                "slug": slug,
                "head": head,
                "guide": guide.as_ref().map(|guide| json!({"url": guide.url, "sha": guide.sha, "at": guide.at})).unwrap_or(Value::Null),
                "current": current,
            })
        );
        return Ok(0);
    }
    if !enabled {
        println!("change guides are off");
    }
    match (guide, current) {
        (Some(guide), Some(true)) => println!("{slug} guide (current): {}", guide.url),
        (Some(guide), _) => println!(
            "{slug} guide (outdated: describes {}, head is {}): {}",
            short(guide.sha.as_deref()),
            short(head.as_deref()),
            guide.url
        ),
        (None, _) => println!("{slug} has no guide recorded"),
    }
    Ok(0)
}

fn is_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

fn short(sha: Option<&str>) -> &str {
    sha.map_or("an unknown commit", |sha| &sha[..sha.len().min(9)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;

    #[tokio::test]
    async fn guide_records_only_when_enabled_and_reads_back_against_head() {
        let mut fixture = CommandFixture::new().await.unwrap();
        let set = |url: &str| GuideArgs {
            positionals: vec![url.into()],
            ..Default::default()
        };
        assert_eq!(
            run(&fixture.ctx, &set("https://example.test/g/1"))
                .await
                .unwrap(),
            2
        );
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .guides
            .enabled = true;
        assert_eq!(run(&fixture.ctx, &set("not a url")).await.unwrap(), 1);
        assert_eq!(
            run(&fixture.ctx, &set("https://example.test/g 1"))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            run(&fixture.ctx, &set("https://example.test/g/1"))
                .await
                .unwrap(),
            0
        );
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let guide = &state["slugs"]["one"]["guide"];
        assert_eq!(guide["url"], "https://example.test/g/1");
        assert!(guide["sha"].as_str().is_some_and(|sha| sha.len() == 40));
        let clear = GuideArgs {
            clear: true,
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &clear).await.unwrap(), 0);
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(state["slugs"]["one"].get("guide").is_none());
        fixture.close().await.unwrap();
    }
}
