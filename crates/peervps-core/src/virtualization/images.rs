//! Guest images for the QEMU backend.
//!
//! An image is a file in the node's image directory, either a qcow2 disk
//! (`<name>.qcow2`) or an installer ISO (`<name>.iso`). Stock cloud images
//! from the catalog below can be pulled with [`pull`] (checksum-verified);
//! any other disk or ISO, a Windows installer included, can be added with
//! [`import`] or dropped into the directory by hand. Disk guests boot from a
//! copy-on-write overlay, so one base image serves any number of VMs; ISO
//! guests boot the installer with a blank disk.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;

use crate::{Error, Result};

/// A cloud image the node knows how to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogImage {
    pub name: &'static str,
    pub title: &'static str,
    #[serde(skip)]
    base_url: &'static str,
    #[serde(skip)]
    file_x86_64: &'static str,
    #[serde(skip)]
    file_aarch64: &'static str,
    #[serde(skip)]
    sums: Sums,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sums {
    Sha256(&'static str),
    Sha512(&'static str),
}

pub const CATALOG: &[CatalogImage] = &[
    CatalogImage {
        name: "ubuntu-24.04",
        title: "Ubuntu 24.04 LTS",
        base_url: "https://cloud-images.ubuntu.com/noble/current/",
        file_x86_64: "noble-server-cloudimg-amd64.img",
        file_aarch64: "noble-server-cloudimg-arm64.img",
        sums: Sums::Sha256("SHA256SUMS"),
    },
    CatalogImage {
        name: "ubuntu-22.04",
        title: "Ubuntu 22.04 LTS",
        base_url: "https://cloud-images.ubuntu.com/jammy/current/",
        file_x86_64: "jammy-server-cloudimg-amd64.img",
        file_aarch64: "jammy-server-cloudimg-arm64.img",
        sums: Sums::Sha256("SHA256SUMS"),
    },
    CatalogImage {
        name: "debian-13",
        title: "Debian 13",
        base_url: "https://cloud.debian.org/images/cloud/trixie/latest/",
        file_x86_64: "debian-13-genericcloud-amd64.qcow2",
        file_aarch64: "debian-13-genericcloud-arm64.qcow2",
        sums: Sums::Sha512("SHA512SUMS"),
    },
];

impl CatalogImage {
    pub fn get(name: &str) -> Option<&'static CatalogImage> {
        CATALOG.iter().find(|i| i.name == name)
    }

    fn file(&self) -> Result<&'static str> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(self.file_x86_64),
            "aarch64" => Ok(self.file_aarch64),
            other => Err(Error::Unsupported(format!("no {} image for {other} hosts", self.name))),
        }
    }
}

/// What an installed image boots as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageKind {
    /// A bootable qcow2 disk (cloud image or a prepared guest).
    Disk,
    /// An installer ISO, booted with a blank disk.
    Iso,
}

impl ImageKind {
    fn ext(self) -> &'static str {
        match self {
            Self::Disk => "qcow2",
            Self::Iso => "iso",
        }
    }
}

/// An image present in the directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalImage {
    pub name: String,
    pub size_bytes: u64,
    pub kind: ImageKind,
    /// For ISOs: what the installer is, read from its volume label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso: Option<IsoInfo>,
}

/// What an installer ISO contains, as far as its ISO 9660 volume label tells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IsoInfo {
    pub label: String,
    pub windows: bool,
    /// CPU architecture the installer is for, when the label says (`x86_64` / `aarch64`).
    pub arch: Option<&'static str>,
}

/// Microsoft's ISO labels: `CCCOMA_X64FRE_EN-US_DV9`, `CPBA_A64FRE_...`, `CCSA_X64FRE_...`, `CENA_X64FREV_...`.
const WINDOWS_LABELS: &[&str] = &["CCCOMA_", "CPBA_", "CCSA_", "CENA_", "J_CCSA", "SSS_", "ESD-ISO", "WIN"];

/// Read the ISO 9660 primary volume descriptor of `path`. `name` (the image
/// name) also counts: a renamed or remastered Windows ISO is still Windows.
pub fn iso_info(path: &Path, name: &str) -> Result<IsoInfo> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let mut pvd = [0u8; 2048];
    f.seek(SeekFrom::Start(16 * 2048))?;
    f.read_exact(&mut pvd).map_err(|_| Error::Invalid(format!("{} is not an ISO image", path.display())))?;
    if &pvd[1..6] != b"CD001" {
        return Err(Error::Invalid(format!("{} is not an ISO image", path.display())));
    }
    let label = String::from_utf8_lossy(&pvd[40..72]).trim().to_owned();
    Ok(classify_iso(label, name))
}

fn classify_iso(label: String, name: &str) -> IsoInfo {
    let upper = label.to_ascii_uppercase();
    let lname = name.to_ascii_lowercase();
    let windows = WINDOWS_LABELS.iter().any(|p| upper.starts_with(p)) || lname.contains("win");
    let has = |needles: &[&str]| needles.iter().any(|n| upper.contains(n) || lname.contains(&n.to_ascii_lowercase()));
    let arch = if has(&["A64", "ARM64", "AARCH64"]) {
        Some("aarch64")
    } else if has(&["X64", "AMD64", "X86_64"]) {
        Some("x86_64")
    } else {
        None
    };
    IsoInfo { label, windows, arch }
}

/// Image names come from renters: allow only a plain file stem.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.len() > 64
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::Invalid(format!("invalid image name {name:?}")));
    }
    Ok(())
}

pub fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.qcow2"))
}

pub fn iso_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.iso"))
}

/// An installed image, located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub kind: ImageKind,
}

/// Path of an installed image (a disk wins over an ISO of the same name), or
/// `NotFound` with a hint on how to get it.
pub fn resolve(dir: &Path, name: &str) -> Result<Resolved> {
    validate_name(name)?;
    for kind in [ImageKind::Disk, ImageKind::Iso] {
        let p = dir.join(format!("{name}.{}", kind.ext()));
        if p.is_file() {
            return Ok(Resolved { path: p, kind });
        }
    }
    let hint = match CatalogImage::get(name) {
        Some(_) => format!("run `peervps image pull {name}`"),
        None => format!(
            "run `peervps image import <file.iso|file.qcow2>` or put {name}.qcow2 / {name}.iso in {}",
            dir.display()
        ),
    };
    Err(Error::NotFound(format!("image {name} is not installed; {hint}")))
}

pub fn list(dir: &Path) -> Result<Vec<LocalImage>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(out) };
    for e in entries.flatten() {
        let p = e.path();
        let kind = match p.extension().and_then(|x| x.to_str()) {
            Some("qcow2") => ImageKind::Disk,
            Some("iso") => ImageKind::Iso,
            _ => continue,
        };
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            && validate_name(stem).is_ok()
            // Shadowed by a disk of the same name (see `resolve`).
            && !(kind == ImageKind::Iso && path(dir, stem).is_file())
        {
            out.push(LocalImage {
                name: stem.to_owned(),
                size_bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
                kind,
                iso: (kind == ImageKind::Iso).then(|| iso_info(&p, stem).ok()).flatten(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// An image name derived from a file name: `Win11_24H2_English_Arm64.iso` → `win11_24h2_english_arm64`.
pub fn name_from_file(file: &Path) -> String {
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let mut name: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_start_matches('.')
        .chars()
        .take(64)
        .collect();
    if name.is_empty() {
        name = "image".into();
    }
    name
}

/// Copy a qcow2 disk or an installer ISO from `src` into `dir` as `name`
/// (default: derived from the file name). Refuses to replace an existing image.
pub async fn import(dir: &Path, src: &Path, name: Option<&str>) -> Result<LocalImage> {
    let kind = match src.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("iso") => ImageKind::Iso,
        Some("qcow2" | "img") => ImageKind::Disk,
        _ => return Err(Error::Invalid(format!("{}: expected a .iso or .qcow2 file", src.display()))),
    };
    let name = name.map(str::to_owned).unwrap_or_else(|| name_from_file(src));
    validate_name(&name)?;
    let iso = match kind {
        ImageKind::Iso => Some(iso_info(src, &name)?),
        ImageKind::Disk => {
            let mut magic = [0u8; 4];
            use std::io::Read;
            std::fs::File::open(src)?.read_exact(&mut magic)?;
            if &magic != b"QFI\xfb" {
                return Err(Error::Invalid(format!("{} is not a qcow2 disk", src.display())));
            }
            None
        }
    };
    if resolve(dir, &name).is_ok() {
        return Err(Error::Invalid(format!("an image named {name} already exists in {}", dir.display())));
    }
    tokio::fs::create_dir_all(dir).await?;
    let dest = dir.join(format!("{name}.{}", kind.ext()));
    let part = dir.join(format!("{name}.{}.part", kind.ext()));
    if let Err(e) = tokio::fs::copy(src, &part).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e.into());
    }
    tokio::fs::rename(&part, &dest).await?;
    let size_bytes = tokio::fs::metadata(&dest).await?.len();
    Ok(LocalImage { name, size_bytes, kind, iso })
}

/// Download a catalog image into `dir`, verify its published checksum, and
/// install it as `<name>.qcow2`. `progress(done, total)` is called per chunk.
pub async fn pull(
    http: &reqwest::Client,
    dir: &Path,
    name: &str,
    mut progress: impl FnMut(u64, Option<u64>) + Send,
) -> Result<PathBuf> {
    let image = CatalogImage::get(name).ok_or_else(|| {
        let names: Vec<&str> = CATALOG.iter().map(|i| i.name).collect();
        Error::NotFound(format!("no catalog image {name:?}; known: {}", names.join(", ")))
    })?;
    let file = image.file()?;
    let net = |e: reqwest::Error| Error::Io(std::io::Error::other(e.to_string()));

    let (sums_file, sha512) = match image.sums {
        Sums::Sha256(f) => (f, false),
        Sums::Sha512(f) => (f, true),
    };
    let sums = http
        .get(format!("{}{sums_file}", image.base_url))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(net)?
        .text()
        .await
        .map_err(net)?;
    let expected = expected_sum(&sums, file)
        .ok_or_else(|| Error::Invalid(format!("{file} is not listed in {sums_file}")))?
        .to_ascii_lowercase();

    tokio::fs::create_dir_all(dir).await?;
    let part = dir.join(format!("{name}.qcow2.part"));
    let mut resp = http
        .get(format!("{}{file}", image.base_url))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(net)?;
    let total = resp.content_length();
    let mut out = tokio::fs::File::create(&part).await?;
    let mut h256 = Sha256::new();
    let mut h512 = Sha512::new();
    let mut done = 0u64;
    while let Some(chunk) = resp.chunk().await.map_err(net)? {
        if sha512 {
            h512.update(&chunk);
        } else {
            h256.update(&chunk);
        }
        out.write_all(&chunk).await?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    out.flush().await?;
    drop(out);
    let got = if sha512 { hex::encode(h512.finalize()) } else { hex::encode(h256.finalize()) };
    if got != expected {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(Error::Crypto(format!("checksum mismatch for {file}: got {got}, want {expected}")));
    }
    let dest = path(dir, name);
    tokio::fs::rename(&part, &dest).await?;
    Ok(dest)
}

/// Find `file`'s digest in a `SHA*SUMS` listing (`<hex> *file` or `<hex>  file`).
fn expected_sum<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|l| {
        let (hash, name) = l.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == file).then_some(hash)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        for bad in ["", ".x", "../etc", "a/b", "a\\b", "x y"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        let dir = std::env::temp_dir().join(format!("pvps-img-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let err = resolve(&dir, "ubuntu-24.04").expect_err("missing");
        assert!(err.to_string().contains("peervps image pull ubuntu-24.04"), "{err}");
        let err = resolve(&dir, "windows-11").expect_err("missing");
        assert!(err.to_string().contains("windows-11.qcow2") && err.to_string().contains("image import"), "{err}");
        std::fs::write(path(&dir, "windows-11"), b"x").expect("write");
        std::fs::write(dir.join("notes.txt"), b"x").expect("write");
        assert_eq!(resolve(&dir, "windows-11").expect("found").path, path(&dir, "windows-11"));
        assert_eq!(
            list(&dir).expect("list"),
            vec![LocalImage { name: "windows-11".into(), size_bytes: 1, kind: ImageKind::Disk, iso: None }]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A minimal ISO 9660 image: 16 empty sectors, then a primary volume descriptor.
    pub(crate) fn fake_iso(path: &Path, label: &str) {
        let mut bytes = vec![0u8; 18 * 2048];
        let pvd = &mut bytes[16 * 2048..17 * 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[40..72].fill(b' ');
        pvd[40..40 + label.len()].copy_from_slice(label.as_bytes());
        std::fs::write(path, bytes).expect("write iso");
    }

    #[tokio::test]
    async fn imports_isos_and_reads_their_labels() {
        let root = std::env::temp_dir().join(format!("pvps-iso-{}", uuid::Uuid::new_v4().simple()));
        let (src, dir) = (root.join("src"), root.join("images"));
        std::fs::create_dir_all(&src).expect("mkdir");
        let iso = src.join("Win11_24H2_English_Arm64.iso");
        fake_iso(&iso, "CPBA_A64FRE_EN-US_DV9");

        let img = import(&dir, &iso, None).await.expect("import");
        assert_eq!(img.name, "win11_24h2_english_arm64");
        assert_eq!(img.kind, ImageKind::Iso);
        let info = img.iso.expect("iso info");
        assert!(info.windows);
        assert_eq!(info.arch, Some("aarch64"));
        let r = resolve(&dir, &img.name).expect("resolve");
        assert_eq!((r.kind, r.path), (ImageKind::Iso, iso_path(&dir, &img.name)));
        assert_eq!(list(&dir).expect("list")[0].iso.as_ref().map(|i| i.label.as_str()), Some("CPBA_A64FRE_EN-US_DV9"));

        let err = import(&dir, &iso, None).await.expect_err("duplicate");
        assert!(err.to_string().contains("already exists"), "{err}");
        let not_iso = src.join("fake.iso");
        std::fs::write(&not_iso, vec![0u8; 40_000]).expect("write");
        assert!(import(&dir, &not_iso, None).await.is_err());
        let txt = src.join("notes.txt");
        std::fs::write(&txt, b"x").expect("write");
        assert!(import(&dir, &txt, None).await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn classifies_installers() {
        let x64 = classify_iso("CCCOMA_X64FRE_EN-US_DV9".into(), "win11");
        assert_eq!((x64.windows, x64.arch), (true, Some("x86_64")));
        let ubuntu = classify_iso("Ubuntu-Server 24.04.1 LTS arm64".into(), "ubuntu-server");
        assert_eq!((ubuntu.windows, ubuntu.arch), (false, Some("aarch64")));
        let renamed = classify_iso("CDROM".into(), "my-windows-10");
        assert_eq!((renamed.windows, renamed.arch), (true, None));
        assert_eq!(name_from_file(Path::new("/x/My Disk (v2).QCOW2")), "my-disk--v2-");
    }

    #[test]
    fn parses_checksum_listings() {
        let ubuntu = "aa11 *noble-server-cloudimg-amd64.img\nbb22 *noble-server-cloudimg-arm64.img\n";
        assert_eq!(expected_sum(ubuntu, "noble-server-cloudimg-arm64.img"), Some("bb22"));
        let debian = "cc33  debian-13-genericcloud-amd64.qcow2\n";
        assert_eq!(expected_sum(debian, "debian-13-genericcloud-amd64.qcow2"), Some("cc33"));
        assert_eq!(expected_sum(debian, "other.qcow2"), None);
    }

    #[test]
    fn catalog_covers_both_architectures() {
        for i in CATALOG {
            assert!(validate_name(i.name).is_ok());
            assert!(i.file_x86_64 != i.file_aarch64);
        }
    }
}
