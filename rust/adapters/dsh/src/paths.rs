use std::{
    collections::{HashMap, HashSet},
    env,
    path::{Path, PathBuf},
};

use ccusage_adapter_common::collect_files_with_extension;
use ccusage_core::Result;

pub(crate) const DSH_HOME_ENV: &str = "DSH_HOME";
pub(crate) const MAX_SUPPORTED_GENERATION: u32 = 3;

fn roots() -> Vec<PathBuf> {
    let configured = env::var(DSH_HOME_ENV).ok().map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    });
    let candidates = configured
        .filter(|paths| !paths.is_empty())
        .unwrap_or_else(|| {
            ccusage_core::home::home_dir()
                .map(|home| vec![home.join(".dsh")])
                .unwrap_or_default()
        });
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SessionFileVersion {
    pub(crate) generation: u32,
    pub(crate) compressed: bool,
}

pub(crate) fn session_file_version(path: &Path) -> Option<SessionFileVersion> {
    let name = path.file_name()?.to_str()?;
    let (stem, compressed) = name
        .strip_suffix(".jsonl.zstd")
        .map(|stem| (stem, true))
        .or_else(|| name.strip_suffix(".jsonl").map(|stem| (stem, false)))?;
    let generation = if stem == "session" {
        0
    } else {
        stem.strip_prefix("session.v")?.parse::<u32>().ok()?
    };
    Some(SessionFileVersion {
        generation,
        compressed,
    })
}

pub(crate) fn discover_session_files() -> Result<Vec<PathBuf>> {
    let mut selected = HashMap::<PathBuf, (SessionFileVersion, PathBuf)>::new();
    for root in roots() {
        let sessions = root.join("sessions");
        let mut candidates = Vec::new();
        collect_files_with_extension(&sessions, "jsonl", &mut candidates);
        collect_files_with_extension(&sessions, "zstd", &mut candidates);
        candidates.sort();
        for path in candidates {
            let Some(version) = session_file_version(&path) else {
                continue;
            };
            let Some(session_dir) = path.parent().map(Path::to_path_buf) else {
                continue;
            };
            let replace = selected
                .get(&session_dir)
                .is_none_or(|(current, _)| version > *current);
            if replace {
                selected.insert(session_dir, (version, path));
            }
        }
    }
    let mut files = selected
        .into_values()
        .filter(|(version, _)| version.generation <= MAX_SUPPORTED_GENERATION)
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        session_file_version(right)
            .map(|version| version.generation)
            .cmp(&session_file_version(left).map(|version| version.generation))
            .then_with(|| left.cmp(right))
    });
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use ccusage_test_support::{EnvVarsGuard, fs_fixture};

    use super::*;

    #[test]
    fn selects_only_the_highest_generation_in_each_session_directory() {
        let fixture = fs_fixture!({
            "sessions/project/session-a/session.jsonl": "{}\n",
            "sessions/project/session-a/session.v2.jsonl.zstd": "{}\n",
            "sessions/project/session-a/session.v3.jsonl.zstd": "{}\n",
            "sessions/project/session-b/session.jsonl": "{}\n",
        });
        let _guard = EnvVarsGuard::set_many([(
            DSH_HOME_ENV,
            Some(OsString::from(fixture.root().as_os_str())),
        )]);

        let files = discover_session_files().unwrap();

        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .any(|path| path.ends_with("session.v3.jsonl.zstd"))
        );
        assert!(files.iter().any(|path| path.ends_with("session.jsonl")));
    }

    #[test]
    fn skips_a_session_whose_highest_generation_is_not_supported() {
        let fixture = fs_fixture!({
            "sessions/project/session-a/session.v3.jsonl": "{}\n",
            "sessions/project/session-a/session.v4.jsonl": "{}\n",
            "sessions/project/session-b/session.v3.jsonl": "{}\n",
        });
        let _guard = EnvVarsGuard::set_many([(
            DSH_HOME_ENV,
            Some(OsString::from(fixture.root().as_os_str())),
        )]);

        let files = discover_session_files().unwrap();

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("session-b/session.v3.jsonl"));
    }

    #[test]
    fn compressed_file_wins_an_ambiguous_same_generation_pair() {
        let fixture = fs_fixture!({
            "sessions/project/session-a/session.v3.jsonl": "{}\n",
            "sessions/project/session-a/session.v3.jsonl.zstd": "{}\n",
        });
        let _guard = EnvVarsGuard::set_many([(
            DSH_HOME_ENV,
            Some(OsString::from(fixture.root().as_os_str())),
        )]);

        let files = discover_session_files().unwrap();

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("session.v3.jsonl.zstd"));
    }

    #[test]
    fn orders_newer_generations_first_across_multiple_roots() {
        let fixture = fs_fixture!({
            "old/sessions/project/session-a/session.jsonl": "{}\n",
            "new/sessions/project/session-a/session.v3.jsonl": "{}\n",
        });
        let roots = format!(
            "{},{}",
            fixture.path("old").display(),
            fixture.path("new").display()
        );
        let _guard = EnvVarsGuard::set_many([(DSH_HOME_ENV, Some(OsString::from(roots)))]);

        let files = discover_session_files().unwrap();

        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("session.v3.jsonl"));
        assert!(files[1].ends_with("session.jsonl"));
    }
}
