//! Desktop editor launchers explicitly transfer ownership of successful child
//! applications. A failed launcher remains an actionable error.

use crate::context::AppContext;
use anyhow::Result;
use std::{path::Path, time::Duration};
use wt_platform::process::CommandSpec;

pub async fn open(context: &AppContext, path: &Path) -> Result<()> {
    let script = command(
        context.config.editor.command.as_deref(),
        &path.to_string_lossy(),
    );
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "sh".into());
    let mut spec = CommandSpec::new(shell)
        .args(["-lc", &script])
        .cwd(&context.config.paths.main_clone);
    spec.preserve_children_on_success = true;
    spec.timeout = Duration::from_secs(15);
    let program = spec.program.clone();
    context
        .processes
        .run(spec, &context.cancellation)
        .await?
        .checked(program)?;
    Ok(())
}

fn command(template: Option<&str>, path: &str) -> String {
    let path = format!("'{}'", path.replace('\'', "'\\''"));
    match template {
        Some(template) if template.contains("{{path}}") => template.replace("{{path}}", &path),
        Some(template) => format!("{template} {path}"),
        None => format!("zed -n {path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::CommandFixture;

    #[tokio::test]
    async fn editor_failures_propagate_and_paths_are_literal_shell_arguments() {
        let mut fixture = CommandFixture::new().await.unwrap();
        let marker = fixture._root.path().join("argv");
        Arc::make_mut(&mut fixture.ctx.config).editor.command =
            Some(format!("printf '%s' {{{{path}}}} > '{}'", marker.display()));
        let literal = "literal ' quote $HOME $(touch unexpected)";
        open(&fixture.ctx, Path::new(literal)).await.unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), literal);
        Arc::make_mut(&mut fixture.ctx.config).editor.command = Some("exit 7 #".into());
        assert!(open(&fixture.ctx, Path::new("ignored")).await.is_err());
        fixture.close().await.unwrap();
    }
    use std::sync::Arc;
}
