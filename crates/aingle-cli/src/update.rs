use std::{
    ffi::OsStr,
    fs::File,
    io::Cursor,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/syi0808/aingle-cli/releases/latest";
const MAX_ARCHIVE_BYTES: usize = 100 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub target: String,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub async fn check() -> Result<UpdateStatus> {
    let release = latest_release().await?;
    status(&release)
}

pub async fn install() -> Result<UpdateStatus> {
    let release = latest_release().await?;
    let status = status(&release)?;
    if !status.update_available {
        return Ok(status);
    }

    let archive_name = format!(
        "aingle-{}-{}.{}",
        status.latest_version,
        status.target,
        archive_extension()
    );
    let checksum_name = format!("{archive_name}.sha256");
    let archive = release_asset(&release, &archive_name)?;
    let checksum = release_asset(&release, &checksum_name)?;
    let client = http_client()?;
    let archive_bytes = download(&client, &archive.browser_download_url, MAX_ARCHIVE_BYTES).await?;
    let checksum_bytes =
        download(&client, &checksum.browser_download_url, MAX_CHECKSUM_BYTES).await?;
    verify_checksum(&archive_name, &archive_bytes, &checksum_bytes)?;

    let directory = tempfile::tempdir().context("create update staging directory")?;
    let binary = extract_binary(&archive_bytes, directory.path())?;
    verify_binary(&binary, &status.latest_version)?;
    self_replace::self_replace(&binary).context("replace the current aingle executable")?;
    Ok(status)
}

async fn latest_release() -> Result<Release> {
    http_client()?
        .get(LATEST_RELEASE_URL)
        .send()
        .await
        .context("check the latest Aingle CLI release")?
        .error_for_status()
        .context("latest Aingle CLI release request failed")?
        .json()
        .await
        .context("decode the latest Aingle CLI release")
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aingle-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(15))
        .build()
        .context("create update HTTP client")
}

fn status(release: &Release) -> Result<UpdateStatus> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let latest_text = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let latest = Version::parse(latest_text)
        .with_context(|| format!("invalid release version {}", release.tag_name))?;
    Ok(UpdateStatus {
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        update_available: latest > current,
        target: target_triple()?.to_owned(),
    })
}

async fn download(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("update download exceeds {limit} bytes");
    }
    let bytes = response.bytes().await.context("read update download")?;
    if bytes.len() > limit {
        bail!("update download exceeds {limit} bytes");
    }
    Ok(bytes.to_vec())
}

fn release_asset<'a>(release: &'a Release, name: &str) -> Result<&'a Asset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| anyhow!("official release asset is unavailable: {name}"))
}

fn verify_checksum(name: &str, archive: &[u8], checksum: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(checksum).context("checksum is not UTF-8")?;
    let mut fields = text.split_whitespace();
    let expected = fields.next().context("checksum is empty")?;
    let listed_name = fields.next().context("checksum filename is missing")?;
    if fields.next().is_some() || listed_name.trim_start_matches('*') != name {
        bail!("checksum does not describe {name}");
    }
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("checksum is not a SHA-256 digest");
    }
    let actual = format!("{:x}", Sha256::digest(archive));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("Aingle CLI checksum mismatch");
    }
    Ok(())
}

fn extract_binary(archive: &[u8], directory: &Path) -> Result<PathBuf> {
    let output = directory.join(binary_name());
    if cfg!(windows) {
        let mut zip = zip::ZipArchive::new(Cursor::new(archive)).context("open update ZIP")?;
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index).context("read update ZIP entry")?;
            if !entry.is_file()
                || Path::new(entry.name()).file_name() != Some(OsStr::new(binary_name()))
            {
                continue;
            }
            let mut file = File::create(&output).context("create staged executable")?;
            std::io::copy(&mut entry, &mut file).context("extract staged executable")?;
            return Ok(output);
        }
    } else {
        let decoder = GzDecoder::new(Cursor::new(archive));
        let mut tar = tar::Archive::new(decoder);
        for entry in tar.entries().context("read update archive")? {
            let mut entry = entry.context("read update archive entry")?;
            if !entry.header().entry_type().is_file()
                || entry.path()?.file_name() != Some(OsStr::new(binary_name()))
            {
                continue;
            }
            let mut file = File::create(&output).context("create staged executable")?;
            std::io::copy(&mut entry, &mut file).context("extract staged executable")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o755))?;
            }
            return Ok(output);
        }
    }
    bail!("release archive does not contain {}", binary_name())
}

fn verify_binary(binary: &Path, version: &str) -> Result<()> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .context("execute staged Aingle CLI")?;
    if !output.status.success() {
        bail!("staged Aingle CLI failed its version check");
    }
    let stdout = String::from_utf8(output.stdout).context("invalid staged version output")?;
    if stdout.trim() != format!("aingle {version}") {
        bail!("staged Aingle CLI reported an unexpected version");
    }
    Ok(())
}

fn archive_extension() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "aingle.exe"
    } else {
        "aingle"
    }
}

fn target_triple() -> Result<&'static str> {
    let environment = if cfg!(target_env = "gnu") {
        "gnu"
    } else if cfg!(target_env = "msvc") {
        "msvc"
    } else {
        ""
    };
    target_for(std::env::consts::OS, std::env::consts::ARCH, environment)
}

fn target_for(os: &str, arch: &str, environment: &str) -> Result<&'static str> {
    match (os, arch, environment) {
        ("linux", "x86_64", _) => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64", _) => Ok("aarch64-unknown-linux-gnu"),
        ("linux", "x86", _) => Ok("i686-unknown-linux-gnu"),
        ("macos", "aarch64", _) => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64", _) => Ok("x86_64-apple-darwin"),
        ("windows", "x86_64", "gnu") => Ok("x86_64-pc-windows-gnu"),
        ("windows", "x86_64", _) => Ok("x86_64-pc-windows-msvc"),
        ("windows", "aarch64", _) => Ok("aarch64-pc-windows-msvc"),
        ("windows", "x86", _) => Ok("i686-pc-windows-msvc"),
        _ => bail!("this platform has no official Aingle CLI update target: {os}/{arch}"),
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::{target_for, verify_checksum};

    #[test]
    fn maps_every_release_target() {
        assert_eq!(
            target_for("linux", "aarch64", "gnu").unwrap(),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            target_for("windows", "x86_64", "gnu").unwrap(),
            "x86_64-pc-windows-gnu"
        );
        assert_eq!(
            target_for("windows", "x86_64", "msvc").unwrap(),
            "x86_64-pc-windows-msvc"
        );
        assert_eq!(
            target_for("macos", "aarch64", "").unwrap(),
            "aarch64-apple-darwin"
        );
    }

    #[test]
    fn accepts_only_matching_named_checksum() {
        let archive = b"release";
        let digest = format!("{:x}", Sha256::digest(archive));
        let checksum = format!("{digest}  aingle.tar.gz\n");
        verify_checksum("aingle.tar.gz", archive, checksum.as_bytes()).unwrap();
        assert!(verify_checksum("other.tar.gz", archive, checksum.as_bytes()).is_err());
        assert!(verify_checksum("aingle.tar.gz", b"changed", checksum.as_bytes()).is_err());
    }
}
