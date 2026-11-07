//! `setup --volume NAME` (macOS): a separate APFS volume for build output,
//! in the system disk's container, so it shares free space with it but is
//! kept out of Time Machine and Spotlight.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::fsutil;

/// What `setup --volume` does, worked out before anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Where the volume is mounted: `/Volumes/NAME`.
    pub mount: PathBuf,
    /// The command creating it, or `None` when it already exists.
    pub create: Option<Vec<String>>,
    /// Commands that need sudo, printed for the user to run.
    pub manual: Vec<String>,
}

/// Rejects names `diskutil` would mangle or that aren't a single path
/// component.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains(':')
        || name.chars().any(char::is_control)
    {
        bail!("{name:?} isn't a usable volume name: use letters, digits, spaces, '-' or '_'");
    }
    Ok(())
}

/// The plan for volume `name`. `container` is the APFS container to add it
/// to (needed only when it doesn't exist yet).
pub fn plan(name: &str, exists: bool, container: Option<&str>) -> Result<Plan> {
    validate_name(name)?;
    let mount = Path::new("/Volumes").join(name);
    let create = if exists {
        None
    } else {
        let container = container.context("couldn't find the system disk's APFS container")?;
        Some(
            ["diskutil", "apfs", "addVolume", container, "APFS", name]
                .map(str::to_owned)
                .to_vec(),
        )
    };
    let quoted = super::shell_quote(&mount);
    Ok(Plan {
        manual: vec![
            format!("sudo mdutil -i off {quoted}"),
            format!("sudo tmutil addexclusion -v {quoted}"),
        ],
        mount,
        create,
    })
}

/// The `APFSContainerReference` (such as `disk3`) in `diskutil info -plist`
/// output.
pub fn container_from_plist(xml: &str) -> Option<String> {
    let after_key = xml.split("<key>APFSContainerReference</key>").nth(1)?;
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let (value, _) = value.split_once("</string>")?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// The APFS container holding the system volume.
fn system_container() -> Result<String> {
    let output = Command::new("diskutil")
        .args(["info", "-plist", "/"])
        .output()
        .context("running diskutil info")?;
    if !output.status.success() {
        bail!(
            "diskutil info -plist / failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    container_from_plist(&String::from_utf8_lossy(&output.stdout))
        .context("diskutil didn't report an APFS container for /")
}

/// Whether `/Volumes/NAME` is a mounted volume. An ordinary directory there
/// is an error: a new volume would be mounted somewhere else.
fn mounted(mount: &Path) -> Result<bool> {
    if !mount.exists() {
        return Ok(false);
    }
    let parent = mount.parent().unwrap_or(Path::new("/"));
    if fsutil::device(mount).is_some() && fsutil::device(mount) != fsutil::device(parent) {
        return Ok(true);
    }
    bail!(
        "{} exists but isn't a mounted volume; remove it or choose another name",
        mount.display()
    )
}

/// Works out and (unless `dry_run`) carries out `setup --volume NAME`,
/// printing each step. Returns the mount point to add as a scan root.
pub fn run(name: &str, dry_run: bool) -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!(
            "--volume creates an APFS volume and works only on macOS. On Linux, put build output \
             on a separate filesystem (a partition, LVM volume or btrfs subvolume) yourself and \
             pass its mount point with --scan-root"
        );
    }
    validate_name(name)?;
    let mount = Path::new("/Volumes").join(name);
    let exists = mounted(&mount)?;
    let container = if exists {
        None
    } else {
        Some(system_container()?)
    };
    let plan = plan(name, exists, container.as_deref())?;
    println!("Build volume: {}", plan.mount.display());
    match &plan.create {
        None => println!("  already exists; using it"),
        Some(command) => {
            println!("  create: {}", command.join(" "));
            if !dry_run {
                let status = Command::new(&command[0])
                    .args(&command[1..])
                    .status()
                    .with_context(|| format!("running {}", command.join(" ")))?;
                if !status.success() {
                    bail!("{} failed ({status})", command.join(" "));
                }
                if !plan.mount.is_dir() {
                    bail!(
                        "diskutil created the volume, but it isn't mounted at {}",
                        plan.mount.display()
                    );
                }
            }
        }
    }
    println!("  then run these yourself (they need sudo):");
    for command in &plan.manual {
        println!("    {command}");
    }
    println!();
    Ok(plan.mount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_a_new_volume_in_the_system_container() {
        let plan = plan("Scratch", false, Some("disk3")).unwrap();
        assert_eq!(plan.mount, PathBuf::from("/Volumes/Scratch"));
        assert_eq!(
            plan.create.unwrap(),
            ["diskutil", "apfs", "addVolume", "disk3", "APFS", "Scratch"]
        );
        assert_eq!(
            plan.manual,
            [
                "sudo mdutil -i off /Volumes/Scratch",
                "sudo tmutil addexclusion -v /Volumes/Scratch"
            ]
        );
    }

    #[test]
    fn reuses_an_existing_volume_and_quotes_names_with_spaces() {
        let plan = plan("Build Output", true, None).unwrap();
        assert_eq!(plan.create, None);
        assert_eq!(plan.manual[0], "sudo mdutil -i off '/Volumes/Build Output'");
        // A new volume needs a container.
        assert!(super::plan("Build Output", false, None).is_err());
    }

    #[test]
    fn rejects_unusable_names() {
        for bad in ["", ".hidden", "a/b", "a:b", "a\nb"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        validate_name("Scratch").unwrap();
        validate_name("cargo_build-2").unwrap();
    }

    #[test]
    fn reads_the_container_from_diskutil_plists() {
        let xml = "<dict>\n\t<key>APFSContainerFree</key>\n\t<integer>1</integer>\n\
                   \t<key>APFSContainerReference</key>\n\t<string>disk3</string>\n</dict>";
        assert_eq!(container_from_plist(xml).as_deref(), Some("disk3"));
        assert_eq!(container_from_plist("<dict></dict>"), None);
    }
}
