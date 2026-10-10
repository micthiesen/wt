//! Desktop editor launchers explicitly transfer ownership of successful child
//! applications. A failed launcher remains an actionable error.

use crate::context::AppContext;
use anyhow::Result;
use std::{path::Path, time::Duration};
use wt_platform::process::CommandSpec;

pub fn remote_uri(host: &str, path: &str) -> Result<String> {
    anyhow::ensure!(
        path.starts_with('/'),
        "remote checkout path must be absolute"
    );
    anyhow::ensure!(
        !host.contains(['/', '?', '#', '%']),
        "SSH host cannot be represented as an editor URI"
    );
    let mut uri = format!("ssh://{host}");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut uri, "%{byte:02X}").expect("write URI to string");
        }
    }
    Ok(uri)
}

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

pub async fn open_url(context: &AppContext, url: &str) -> Result<()> {
    anyhow::ensure!(
        url.starts_with("https://")
            || url.starts_with("http://")
            || url.starts_with("linear://review/"),
        "refusing to open a URL with an unsupported scheme"
    );
    #[cfg(target_os = "macos")]
    let command = CommandSpec::new("open");
    #[cfg(not(target_os = "macos"))]
    let command = CommandSpec::new("xdg-open");
    let mut command = command.args([url]);
    command.preserve_children_on_success = true;
    let name = command.program.clone();
    context
        .processes
        .run(command, &context.cancellation)
        .await?
        .checked(name)?;
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

    #[test]
    fn remote_paths_cannot_change_uri_authority_query_or_fragment() {
        assert_eq!(
            remote_uri("user@builder", "/work/space #percent%/é").unwrap(),
            "ssh://user@builder/work/space%20%23percent%25/%C3%A9"
        );
        assert!(remote_uri("builder", "relative").is_err());
        assert!(remote_uri("builder/path", "/work").is_err());
    }

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
