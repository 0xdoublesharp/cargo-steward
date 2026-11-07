//! Scan roots for a first `setup`: where this machine already puts build
//! output, so cleanup finds build directories the wrapper hasn't seen yet.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use toml_edit::DocumentMut;

use crate::config::Paths;
use crate::{fsutil, ledger, scan};

/// A detected scan root and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub root: PathBuf,
    pub reason: String,
}

/// The fixed part of a `build.build-dir` or `build.target-dir` value: the
/// path up to the first component holding a `{template}`, with
/// `{cargo-cache-home}` expanded. `None` for relative paths and ones that
/// depend on the workspace (`{workspace-root}/target`, cargo's default).
pub fn template_root(value: &str, cargo_home: &Path) -> Option<PathBuf> {
    let expanded = value.replace("{cargo-cache-home}", &cargo_home.to_string_lossy());
    let path = Path::new(&expanded);
    if !path.is_absolute() {
        return None;
    }
    let mut root = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(name) = component
            && name.to_string_lossy().contains('{')
        {
            break;
        }
        root.push(component);
    }
    // Something more specific than the filesystem root.
    (root.components().count() > 1).then_some(root)
}

/// The mount point of the volume holding `path`: the highest ancestor still
/// on the same device.
pub fn mount_point(path: &Path) -> Option<PathBuf> {
    let device = fsutil::device(path)?;
    path.ancestors()
        .take_while(|a| fsutil::device(a) == Some(device))
        .last()
        .map(Path::to_path_buf)
}

/// Build directories the wrapper has recorded: in the registry, and in the
/// ledger not yet compacted into it.
pub fn recorded_build_dirs(paths: &Paths) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ledger::load(paths).roots.into_keys().collect();
    if let Ok(text) = fs::read_to_string(paths.ledger()) {
        dirs.extend(
            text.lines()
                .filter_map(|l| serde_json::from_str::<ledger::Record>(l).ok())
                .map(|r| r.root),
        );
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// What the environment and Cargo's user config say about where build
/// output goes.
#[derive(Debug, Default)]
pub struct Sources<'a> {
    /// `$CARGO_TARGET_DIR`.
    pub target_dir_env: Option<PathBuf>,
    /// The user-wide Cargo config.
    pub cargo_config: Option<&'a DocumentMut>,
    pub cargo_home: PathBuf,
    /// Build directories the wrapper has recorded.
    pub recorded: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

/// Scan roots worth adding, from (in order): the parent of
/// `$CARGO_TARGET_DIR`, the fixed part of `build.build-dir` and
/// `build.target-dir`, and the volumes (other than the home directory's)
/// holding recorded build directories. Only existing directories count;
/// nested ones fold into the outer one, and the filesystem root, the home
/// directory and its ancestors are left out as too broad to search.
pub fn detect(sources: &Sources) -> Vec<Detected> {
    let mut found: Vec<Detected> = Vec::new();
    if let Some(dir) = &sources.target_dir_env
        && let Some(parent) = dir.parent()
    {
        found.push(Detected {
            root: parent.to_path_buf(),
            reason: format!("parent of CARGO_TARGET_DIR={}", dir.display()),
        });
    }
    if let Some(doc) = sources.cargo_config {
        for key in ["build-dir", "target-dir"] {
            if let Some(value) = doc
                .get("build")
                .and_then(|b| b.get(key))
                .and_then(|v| v.as_str())
                && let Some(root) = template_root(value, &sources.cargo_home)
            {
                found.push(Detected {
                    root,
                    reason: format!("build.{key} = {value:?} in Cargo's user config"),
                });
            }
        }
    }
    let home_device = sources.home.as_deref().and_then(fsutil::device);
    let mut volumes: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for dir in &sources.recorded {
        let Some(device) = fsutil::device(dir) else {
            continue;
        };
        if Some(device) == home_device {
            continue;
        }
        if let Some(mount) = mount_point(dir) {
            *volumes.entry(mount).or_default() += 1;
        }
    }
    for (mount, count) in volumes {
        found.push(Detected {
            root: mount,
            reason: format!(
                "volume holding {count} recorded build director{}",
                if count == 1 { "y" } else { "ies" }
            ),
        });
    }
    finalize(found, sources.home.as_deref())
}

/// Canonicalizes, drops what doesn't exist or is too broad, and keeps only
/// the outermost of nested roots (with the reason it was first found for).
fn finalize(found: Vec<Detected>, home: Option<&Path>) -> Vec<Detected> {
    let home = home.and_then(|h| h.canonicalize().ok());
    let mut by_root: BTreeMap<PathBuf, String> = BTreeMap::new();
    for detected in found {
        let Ok(root) = detected.root.canonicalize() else {
            continue;
        };
        if !root.is_dir() || root.parent().is_none() {
            continue;
        }
        if home.as_deref().is_some_and(|h| h.starts_with(&root)) {
            continue;
        }
        by_root.entry(root).or_insert(detected.reason);
    }
    let kept = scan::outermost(by_root.keys().cloned().collect());
    kept.into_iter()
        .filter_map(|root| {
            let reason = by_root.remove(&root)?;
            Some(Detected { root, reason })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_fixed_part_of_build_dir_templates() {
        let home = Path::new("/home/u/.cargo");
        assert_eq!(
            template_root("/Volumes/Scratch/cargo-build/{workspace-path-hash}", home),
            Some(PathBuf::from("/Volumes/Scratch/cargo-build"))
        );
        assert_eq!(
            template_root("{cargo-cache-home}/build/{workspace-path-hash}", home),
            Some(PathBuf::from("/home/u/.cargo/build"))
        );
        assert_eq!(
            template_root("/srv/target", home),
            Some(PathBuf::from("/srv/target"))
        );
        assert_eq!(template_root("{workspace-root}/target", home), None);
        assert_eq!(template_root("target", home), None);
        assert_eq!(template_root("/{workspace-path-hash}", home), None);
    }

    #[test]
    fn finds_the_mount_point_of_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        let mount = mount_point(&deep).unwrap();
        assert!(deep.starts_with(&mount));
        assert_eq!(fsutil::device(&mount), fsutil::device(&deep));
        if let Some(parent) = mount.parent() {
            assert_ne!(fsutil::device(parent), fsutil::device(&deep));
        }
    }

    #[test]
    fn detects_roots_from_the_environment_and_cargo_config() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let scratch = base.join("scratch");
        fs::create_dir_all(scratch.join("cargo/proj/main")).unwrap();
        fs::create_dir_all(base.join("builds")).unwrap();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        let doc: DocumentMut = format!(
            "[build]\nbuild-dir = \"{}/builds/{{workspace-path-hash}}\"\ntarget-dir = \"{}/missing\"\n",
            base.display(),
            base.display()
        )
        .parse()
        .unwrap();
        let sources = Sources {
            target_dir_env: Some(scratch.join("cargo/proj/main")),
            cargo_config: Some(&doc),
            cargo_home: home.join(".cargo"),
            recorded: Vec::new(),
            home: Some(home.clone()),
        };
        let detected = detect(&sources);
        let roots: Vec<&Path> = detected.iter().map(|d| d.root.as_path()).collect();
        // The missing target-dir is skipped.
        assert_eq!(roots, [base.join("builds"), scratch.join("cargo/proj")]);
        assert!(detected[0].reason.contains("build.build-dir"));
        assert!(detected[1].reason.contains("CARGO_TARGET_DIR"));
    }

    #[test]
    fn keeps_outer_roots_and_skips_broad_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let home = base.join("home/u");
        fs::create_dir_all(home.join("projects")).unwrap();
        fs::create_dir_all(base.join("vol/cargo/x")).unwrap();
        let found = vec![
            Detected {
                root: base.join("vol/cargo/x"),
                reason: "inner".into(),
            },
            Detected {
                root: base.join("vol"),
                reason: "outer".into(),
            },
            Detected {
                root: base.join("home"),
                reason: "contains home".into(),
            },
            Detected {
                root: PathBuf::from("/"),
                reason: "root".into(),
            },
            Detected {
                root: home.join("projects"),
                reason: "under home".into(),
            },
        ];
        let kept = finalize(found, Some(&home));
        assert_eq!(
            kept,
            [
                Detected {
                    root: home.join("projects"),
                    reason: "under home".into()
                },
                Detected {
                    root: base.join("vol"),
                    reason: "outer".into()
                },
            ]
        );
    }

    #[test]
    fn reads_recorded_dirs_from_the_registry_and_the_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: tmp.path().join("home"),
        };
        assert!(recorded_build_dirs(&paths).is_empty());
        let record = |root: &str| ledger::Record {
            ts: 1,
            root: root.into(),
            workspace: None,
            ms: 0,
        };
        ledger::append(&paths, &record("/b/one"));
        ledger::compact(&paths).unwrap();
        ledger::append(&paths, &record("/b/two"));
        ledger::append(&paths, &record("/b/one"));
        fs::write(
            paths.ledger(),
            format!("{}not json\n", fs::read_to_string(paths.ledger()).unwrap()),
        )
        .unwrap();
        assert_eq!(
            recorded_build_dirs(&paths),
            [PathBuf::from("/b/one"), PathBuf::from("/b/two")]
        );
    }

    #[test]
    fn detects_volumes_holding_recorded_build_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let (one, two) = (base.join("one"), base.join("two"));
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        let recorded = vec![one.clone(), two.clone(), base.join("gone")];

        // On the home directory's volume: too broad, left out.
        let same = Sources {
            recorded: recorded.clone(),
            home: Some(base.clone()),
            ..Sources::default()
        };
        assert!(detect(&same).is_empty());

        // On another volume than home: that volume's mount point.
        let elsewhere = Path::new("/");
        if fsutil::device(elsewhere) == fsutil::device(&base) {
            return;
        }
        let mount = mount_point(&one).unwrap().canonicalize().unwrap();
        let detected = detect(&Sources {
            recorded,
            home: Some(elsewhere.to_path_buf()),
            ..Sources::default()
        });
        assert_eq!(
            detected,
            [Detected {
                root: mount,
                reason: "volume holding 2 recorded build directories".into()
            }]
        );
        let detected = detect(&Sources {
            recorded: vec![one],
            home: Some(elsewhere.to_path_buf()),
            ..Sources::default()
        });
        assert_eq!(
            detected[0].reason,
            "volume holding 1 recorded build directory"
        );
    }
}
