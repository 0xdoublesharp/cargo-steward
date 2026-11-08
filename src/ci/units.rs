//! Build units as `trim` removes them: the old layout's files grouped by
//! unit hash, the new layout's unit directories, and incremental caches.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::fsutil::{self, entries, subdirs, touched};
use crate::scan::{self, is_hash};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Legacy,
    V2,
    Incremental,
}

/// Something `trim` keeps or removes as a whole.
#[derive(Debug, Clone)]
pub struct Unit {
    pub kind: Kind,
    /// The package (`serde_json`, `proc-macro2`), or for an incremental
    /// cache, the crate.
    pub package: String,
    /// Crate names the unit's outputs carry (`proc_macro2`,
    /// `build_script_build`), which name its incremental caches.
    pub crates: BTreeSet<String>,
    /// Everything that makes up the unit. A fingerprint directory comes
    /// first, so cargo never sees a fingerprint whose outputs are gone.
    pub paths: Vec<PathBuf>,
    /// Where cargo keeps the unit's fingerprint; `None` for incremental
    /// caches and for old-layout outputs whose fingerprint is gone.
    pub fingerprint: Option<PathBuf>,
    pub last_used: SystemTime,
}

impl Unit {
    /// Observed direct-compiler CPU cost, used only as an eviction hint.
    pub fn rebuild_ms(&self) -> Option<u64> {
        let mut costs = Vec::new();
        for path in &self.paths {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(crate::costs::PREFIX))
            {
                costs.extend(crate::costs::read(path).and_then(|o| o.rebuild_ms()));
            } else if self.kind == Kind::V2 {
                costs.extend(
                    crate::costs::in_dir(&path.join("out"))
                        .into_iter()
                        .filter_map(|(_, o)| o.rebuild_ms()),
                );
            } else if path.is_dir() {
                costs.extend(
                    crate::costs::in_dir(path)
                        .into_iter()
                        .filter_map(|(_, o)| o.rebuild_ms()),
                );
            }
        }
        costs.into_iter().max().filter(|&n| n > 0)
    }

    /// Space removing the unit gives back.
    pub fn bytes(&self) -> u64 {
        let mut files = Vec::new();
        let mut stack = self.paths.clone();
        while let Some(path) = stack.pop() {
            match fs::symlink_metadata(&path) {
                Ok(m) if m.is_dir() => stack.extend(entries(&path).map(|(_, p)| p)),
                Ok(_) => files.push(path),
                Err(_) => {}
            }
        }
        fsutil::freed_by(&files)
    }

    pub fn noun(&self) -> &'static str {
        match self.kind {
            Kind::Incremental => "incremental cache",
            Kind::Legacy | Kind::V2 => "unit",
        }
    }
}

/// Reverse dependency edges, using Cargo's fingerprint identities rather
/// than package names (features, profiles and build scripts are distinct).
pub fn dependents(units: &[&Unit]) -> Vec<Vec<usize>> {
    let links: Vec<_> = units
        .iter()
        .map(|u| {
            u.fingerprint
                .as_deref()
                .map(scan::fingerprint_links)
                .unwrap_or_default()
        })
        .collect();
    let mut producers: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, (hashes, _, _)) in links.iter().enumerate() {
        for hash in hashes {
            producers.entry(*hash).or_default().push(i);
        }
    }
    let mut reverse = vec![Vec::new(); units.len()];
    for (i, (_, deps, _)) in links.iter().enumerate() {
        for producer in deps.iter().filter_map(|h| producers.get(h)).flatten() {
            if *producer != i {
                reverse[*producer].push(i);
            }
        }
    }
    for edges in &mut reverse {
        edges.sort_unstable();
        edges.dedup();
    }
    reverse
}

/// Fingerprint directories of a profile, in both layouts.
pub fn fingerprint_dirs(profile: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = subdirs(&profile.join(".fingerprint"))
        .filter(|d| hashed_dir(d).is_some())
        .collect();
    dirs.extend(
        scan::v2_units(profile)
            .into_iter()
            .map(|u| u.path.join("fingerprint")),
    );
    dirs
}

/// `<package>-<hash>` of an old-layout unit directory.
fn hashed_dir(dir: &Path) -> Option<(String, String)> {
    let name = dir.file_name()?.to_str()?;
    let (package, hash) = name.rsplit_once('-')?;
    is_hash(hash).then(|| (package.to_owned(), hash.to_owned()))
}

/// Splits an old-layout output name, `lib<crate>-<hash>.rlib` or
/// `<crate>-<hash>.<cgu>.<invocation>.rcgu.o`, at its unit hash. Crate names
/// have no dashes, so the first `-<16 hex digits>` ending the name or
/// followed by a dot is the hash.
pub fn split_hashed(name: &str) -> Option<(&str, &str)> {
    name.match_indices('-').find_map(|(i, _)| {
        let hash = name.get(i + 1..i + 17)?;
        let rest = &name[i + 17..];
        (is_hash(hash) && (rest.is_empty() || rest.starts_with('.'))).then(|| (&name[..i], hash))
    })
}

/// Crate names from the dep-info files directly in `dir`: `<crate>-<hash>.d`
/// in the old layout, `<crate>.d` or `<crate>-<hash>.d` in the new one.
fn crates_in(dir: &Path, crates: &mut BTreeSet<String>) {
    for (name, _) in entries(dir) {
        if let Some(stem) = name.strip_suffix(".d") {
            let name = split_hashed(stem).map_or(stem, |(name, _)| name);
            crates.insert(name.to_owned());
        }
    }
}

/// Old-layout units: every file and directory carrying the same unit hash,
/// the way cargo-quota groups them: `.fingerprint/<pkg>-<hash>/`,
/// `build/<pkg>-<hash>/`, and `deps/*-<hash>*` (and `examples/`).
pub fn legacy_units(profile: &Path) -> Vec<Unit> {
    #[derive(Default)]
    struct Group {
        package: String,
        fingerprint: Option<PathBuf>,
        others: Vec<PathBuf>,
        crates: BTreeSet<String>,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for dir in subdirs(&profile.join(".fingerprint")) {
        if let Some((package, hash)) = hashed_dir(&dir) {
            let group = groups.entry(hash).or_default();
            group.package = package;
            group.fingerprint = Some(dir);
        }
    }
    for dir in subdirs(&profile.join("build")) {
        if let Some((package, hash)) = hashed_dir(&dir) {
            let group = groups.entry(hash).or_default();
            if group.package.is_empty() {
                group.package = package;
            }
            crates_in(&dir, &mut group.crates);
            group.others.push(dir);
        }
    }
    for dir in ["deps", "examples"] {
        for (name, path) in entries(&profile.join(dir)) {
            let Some((stem, hash)) = split_hashed(&name) else {
                continue;
            };
            let group = groups.entry(hash.to_owned()).or_default();
            if name.ends_with(".d") {
                group.crates.insert(stem.to_owned());
            }
            group.others.push(path);
        }
    }

    groups
        .into_values()
        .map(|g| {
            let last_used = match &g.fingerprint {
                Some(dir) => newest(std::iter::once(dir.clone()).chain(entries(dir).map(|e| e.1))),
                None => newest(g.others.iter().cloned()),
            };
            let mut crates = g.crates;
            if !g.package.is_empty() {
                crates.insert(g.package.replace('-', "_"));
            }
            Unit {
                kind: Kind::Legacy,
                package: g.package,
                crates,
                paths: g.fingerprint.iter().cloned().chain(g.others).collect(),
                fingerprint: g.fingerprint,
                last_used,
            }
        })
        .collect()
}

/// New-layout units: `build/<package>/<hash>/`.
pub fn v2_units(profile: &Path) -> Vec<Unit> {
    scan::v2_units(profile)
        .into_iter()
        .map(|unit| {
            let package = unit
                .path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut crates = BTreeSet::from([package.replace('-', "_")]);
            crates_in(&unit.path.join("out"), &mut crates);
            Unit {
                kind: Kind::V2,
                package,
                crates,
                fingerprint: Some(unit.path.join("fingerprint")),
                paths: vec![unit.path],
                last_used: unit.last_used,
            }
        })
        .collect()
}

/// Incremental caches, `incremental/<crate>-<id>/`, each removed whole.
pub fn incremental_units(profile: &Path) -> Vec<Unit> {
    scan::incremental_dirs(profile)
        .into_iter()
        .map(|cache| {
            let name = cache
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let package = name
                .rsplit_once('-')
                .map_or(name.as_str(), |(krate, _)| krate)
                .to_owned();
            Unit {
                kind: Kind::Incremental,
                crates: BTreeSet::from([package.clone()]),
                package,
                paths: vec![cache.path],
                fingerprint: None,
                last_used: cache.last_used,
            }
        })
        .collect()
}

/// Newest [`touched`] time among `paths`.
fn newest(paths: impl Iterator<Item = PathBuf>) -> SystemTime {
    paths
        .filter_map(|p| fs::symlink_metadata(p).ok())
        .map(|m| touched(&m))
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_edges_follow_fingerprints_across_both_layouts() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("debug/.fingerprint/dep-1111111111111111");
        let new = temp
            .path()
            .join("debug/build/member/2222222222222222/fingerprint");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("lib-dep"), "0100000000000000").unwrap();
        fs::write(new.join("lib-member"), "0200000000000000").unwrap();
        fs::write(
            new.join("lib-member.json"),
            r#"{"deps":[[0,"dep",false,1]]}"#,
        )
        .unwrap();
        let make = |fingerprint: PathBuf, kind| Unit {
            kind,
            package: "same-name".into(),
            crates: BTreeSet::new(),
            paths: vec![],
            fingerprint: Some(fingerprint),
            last_used: SystemTime::UNIX_EPOCH,
        };
        let a = make(old, Kind::Legacy);
        let b = make(new, Kind::V2);
        assert_eq!(dependents(&[&a, &b]), vec![vec![1], vec![]]);
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    fn names(paths: &[PathBuf], profile: &Path) -> Vec<String> {
        let mut names: Vec<String> = paths
            .iter()
            .map(|p| p.strip_prefix(profile).unwrap().display().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn splits_output_names_at_the_unit_hash() {
        let h = "0123456789abcdef";
        for (name, expected) in [
            (format!("libserde-{h}.rlib"), Some("libserde")),
            (format!("serde-{h}.d"), Some("serde")),
            (format!("app-{h}"), Some("app")),
            (format!("app-{h}.dSYM"), Some("app")),
            (format!("app-{h}.0a1b.0c2d.rcgu.o"), Some("app")),
            (
                format!("build_script_build-{h}"),
                Some("build_script_build"),
            ),
            ("libserde.rlib".to_owned(), None),
            (format!("app-{h}0"), None),
            ("app-0123456789ABCDEF".to_owned(), None),
            ("x-0123".to_owned(), None),
        ] {
            assert_eq!(split_hashed(&name), expected.map(|n| (n, h)), "{name}");
        }
    }

    #[test]
    fn old_layout_names_without_a_hash_belong_to_no_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("debug");
        let hash = "5555555555555555";
        for path in [
            // A build script directory whose fingerprint is gone.
            format!("build/left-over-{hash}/out/x"),
            "build/plain/x".to_owned(),
            "deps/plain.txt".to_owned(),
        ] {
            touch(&profile.join(path));
        }
        let units = legacy_units(&profile);
        assert_eq!(units.len(), 1, "{units:?}");
        assert_eq!(units[0].package, "left-over");
        assert_eq!(units[0].fingerprint, None);
        assert!(units[0].crates.contains("left_over"));
        assert_eq!(
            names(&units[0].paths, &profile),
            [format!("build/left-over-{hash}")]
        );
    }

    #[test]
    fn groups_old_layout_files_by_unit_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("debug");
        let lib = "1111111111111111";
        let script = "2222222222222222";
        let run = "3333333333333333";
        let orphan = "4444444444444444";
        for path in [
            format!(".fingerprint/proc-macro2-{lib}/lib-proc_macro2"),
            format!("deps/libproc_macro2-{lib}.rlib"),
            format!("deps/libproc_macro2-{lib}.rmeta"),
            format!("deps/proc_macro2-{lib}.d"),
            format!("deps/proc_macro2-{lib}.a1.b2.rcgu.o"),
            format!(".fingerprint/proc-macro2-{script}/build-script-build-script-build"),
            format!("build/proc-macro2-{script}/build-script-build"),
            format!("build/proc-macro2-{script}/build_script_build-{script}.d"),
            format!(".fingerprint/proc-macro2-{run}/run-build-script-build-script-build"),
            format!("build/proc-macro2-{run}/out/generated.rs"),
            format!("deps/gone-{orphan}"),
            // Not a unit's: outputs copied up, caches, locks.
            "proc_macro2.d".to_owned(),
            "incremental/proc_macro2-1abc/s-x/dep-graph.bin".to_owned(),
            ".cargo-lock".to_owned(),
        ] {
            touch(&profile.join(path));
        }

        let mut units = legacy_units(&profile);
        units.sort_by(|a, b| a.paths[0].cmp(&b.paths[0]));
        assert_eq!(units.len(), 4);
        let [lib_unit, script_unit, run_unit, stray]: &[Unit; 4] = units[..].try_into().unwrap();

        assert_eq!(
            names(&stray.paths, &profile),
            [format!("deps/gone-{orphan}")]
        );
        assert_eq!(stray.fingerprint, None);

        assert_eq!(lib_unit.package, "proc-macro2");
        assert_eq!(
            lib_unit.paths[0],
            profile.join(format!(".fingerprint/proc-macro2-{lib}"))
        );
        assert_eq!(
            names(&lib_unit.paths, &profile),
            [
                format!(".fingerprint/proc-macro2-{lib}"),
                format!("deps/libproc_macro2-{lib}.rlib"),
                format!("deps/libproc_macro2-{lib}.rmeta"),
                format!("deps/proc_macro2-{lib}.a1.b2.rcgu.o"),
                format!("deps/proc_macro2-{lib}.d"),
            ]
        );
        assert_eq!(lib_unit.crates, BTreeSet::from(["proc_macro2".to_owned()]));

        assert_eq!(
            names(&script_unit.paths, &profile),
            [
                format!(".fingerprint/proc-macro2-{script}"),
                format!("build/proc-macro2-{script}"),
            ]
        );
        assert!(script_unit.crates.contains("build_script_build"));
        assert_eq!(
            names(&run_unit.paths, &profile),
            [
                format!(".fingerprint/proc-macro2-{run}"),
                format!("build/proc-macro2-{run}"),
            ]
        );
        assert!(lib_unit.bytes() > 0);

        assert_eq!(fingerprint_dirs(&profile).len(), 3);
        let caches = incremental_units(&profile);
        assert_eq!(caches.len(), 1);
        assert_eq!(caches[0].package, "proc_macro2");
    }

    #[test]
    fn reads_new_layout_units() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("debug");
        let unit = profile.join("build/a-dep/0123456789abcdef");
        touch(&unit.join("fingerprint/lib-a_dep"));
        touch(&unit.join("out/a_dep-0123456789abcdef.d"));
        touch(&profile.join("build/app/fedcba9876543210/out/app.d"));
        fs::create_dir_all(profile.join("build/app/fedcba9876543210/fingerprint")).unwrap();

        let mut units = v2_units(&profile);
        units.sort_by(|a, b| a.package.cmp(&b.package));
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].package, "a-dep");
        assert_eq!(units[0].paths, std::slice::from_ref(&unit));
        assert_eq!(units[0].fingerprint, Some(unit.join("fingerprint")));
        assert_eq!(units[0].crates, BTreeSet::from(["a_dep".to_owned()]));
        assert_eq!(units[1].package, "app");
        assert_eq!(fingerprint_dirs(&profile).len(), 2);
    }
}
