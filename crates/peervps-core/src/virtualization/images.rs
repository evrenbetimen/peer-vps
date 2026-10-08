//! Guest disk images for the QEMU backend.
//!
//! An image is a qcow2 file named `<name>.qcow2` in the node's image
//! directory. Stock cloud images from the catalog below can be pulled with
//! [`pull`] (checksum-verified); anything else, a Windows guest included, can
//! be dropped into the directory by hand. Guests boot from a copy-on-write
//! overlay, so one base image serves any number of VMs.

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

/// An image present in the directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalImage {
    pub name: String,
    pub size_bytes: u64,
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

/// Path of an installed image, or `NotFound` with a hint on how to get it.
pub fn resolve(dir: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let p = path(dir, name);
    if p.is_file() {
        return Ok(p);
    }
    let hint = match CatalogImage::get(name) {
        Some(_) => format!("run `peervps image pull {name}`"),
        None => format!("put a qcow2 disk at {}", p.display()),
    };
    Err(Error::NotFound(format!("image {name} is not installed; {hint}")))
}

pub fn list(dir: &Path) -> Result<Vec<LocalImage>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(out) };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "qcow2")
            && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            && validate_name(stem).is_ok()
        {
            out.push(LocalImage { name: stem.to_owned(), size_bytes: e.metadata().map(|m| m.len()).unwrap_or(0) });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
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
mod tests {
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
        assert!(err.to_string().contains("windows-11.qcow2"), "{err}");
        std::fs::write(path(&dir, "windows-11"), b"x").expect("write");
        std::fs::write(dir.join("notes.txt"), b"x").expect("write");
        assert_eq!(resolve(&dir, "windows-11").expect("found"), path(&dir, "windows-11"));
        assert_eq!(list(&dir).expect("list"), vec![LocalImage { name: "windows-11".into(), size_bytes: 1 }]);
        let _ = std::fs::remove_dir_all(dir);
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
