//! Match configured copy globs without visiting provably unrelated subtrees.
use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

pub(crate) struct CopyGlobs {
    matches: GlobSet,
    prefixes: Vec<String>,
}

impl CopyGlobs {
    pub(crate) fn new(patterns: &[String]) -> Result<Self, globset::Error> {
        let mut builder = GlobSetBuilder::new();
        let mut prefixes = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            builder.add(GlobBuilder::new(pattern).literal_separator(false).build()?);
            // Stop before every glob construct, including escapes. The literal
            // prefix is necessary, never sufficient, for a match. In particular
            // `*` may span separators: do not prune using component depth.
            let end = pattern
                .find(['*', '?', '[', '{', '\\'])
                .unwrap_or(pattern.len());
            prefixes.push(pattern[..end].to_owned());
        }
        Ok(Self {
            matches: builder.build()?,
            prefixes,
        })
    }

    pub(crate) fn is_match(&self, path: &Path) -> bool {
        self.matches.is_match(path)
    }

    pub(crate) fn can_match_descendant(&self, directory: &Path) -> bool {
        let Some(directory) = directory.to_str() else {
            // A lossy representation cannot prove that a subtree is irrelevant.
            return true;
        };
        let directory = format!("{directory}/");
        self.prefixes
            .iter()
            .any(|prefix| prefix.starts_with(&directory) || directory.starts_with(prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_globs_skip_dependency_trees_without_hardcoded_exclusions() {
        let globs = CopyGlobs::new(&[".env*".into(), "packages/config/*.json".into()]).unwrap();
        assert!(!globs.can_match_descendant(Path::new("node_modules")));
        assert!(!globs.can_match_descendant(Path::new("packages/configuration")));
        assert!(globs.can_match_descendant(Path::new("packages")));
        assert!(globs.can_match_descendant(Path::new("packages/config")));
        assert!(globs.can_match_descendant(Path::new("packages/config/nested")));
        assert!(globs.can_match_descendant(Path::new(".env-directory")));
        let dependencies = CopyGlobs::new(&["node_modules/pkg/*.json".into()]).unwrap();
        assert!(dependencies.can_match_descendant(Path::new("node_modules")));
    }

    #[test]
    fn pruning_never_excludes_a_matching_glob_ancestor() {
        let patterns = [
            ".env*",
            "packages/config/*.json",
            "**/.env",
            "*.json",
            "{packages,apps}/**/*.json",
            "packages/[ab]/*.json",
            "foo?bar/*.json",
            r"literal\*/**",
            "config/exact.json",
            "config/**/file.json",
        ];
        let paths = [
            ".env.local",
            ".env-directory/nested/file",
            "packages/config/nested/file.json",
            "packages/configuration/file.json",
            "apps/a/.env",
            "node_modules/pkg/file.json",
            "packages/a/file.json",
            "fooxbar/deep/file.json",
            "literal*/a/file.json",
            "config/exact.json",
            "config/a/b/file.json",
        ];
        for pattern in patterns {
            let globs = CopyGlobs::new(&[pattern.into()]).unwrap();
            for path in paths.map(Path::new) {
                if globs.is_match(path) {
                    for ancestor in path
                        .ancestors()
                        .skip(1)
                        .filter(|p| !p.as_os_str().is_empty())
                    {
                        assert!(
                            globs.can_match_descendant(ancestor),
                            "{pattern}: {path:?} under {ancestor:?}"
                        );
                    }
                }
            }
        }
    }
}
