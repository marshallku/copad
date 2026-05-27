use std::fs;
use std::io::Read;
use std::path::PathBuf;

use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;

const REPO: &str = "marshallku/copad";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(format!("copad/{CURRENT_VERSION}"))
        .build()
        .expect("failed to build HTTP client")
}

fn fetch_release(version: Option<&str>) -> Result<GitHubRelease, String> {
    let url = match version {
        Some(v) => format!("https://api.github.com/repos/{REPO}/releases/tags/{v}"),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };

    client()
        .get(&url)
        .send()
        .map_err(|e| format!("failed to fetch release: {e}"))?
        .json::<GitHubRelease>()
        .map_err(|e| format!("failed to parse release: {e}"))
}

fn parse_version(tag: &str) -> Result<Version, String> {
    let v = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(v).map_err(|e| format!("invalid version '{tag}': {e}"))
}

fn install_dir() -> PathBuf {
    std::env::current_exe()
        .expect("failed to get current executable path")
        .parent()
        .expect("executable has no parent directory")
        .to_path_buf()
}

pub fn check_update() {
    let current = match parse_version(CURRENT_VERSION) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let release = match fetch_release(None) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let latest = match parse_version(&release.tag_name) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    if latest > current {
        println!("Update available: v{current} -> v{latest}");
        println!("Run `coctl update apply` to install.");
    } else {
        println!("Already up to date (v{current}).");
    }
}

pub fn apply_update(version: Option<String>) {
    let release = match fetch_release(version.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let target_version = match parse_version(&release.tag_name) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let asset_name = format!("copad-{}-x86_64-linux.tar.gz", release.tag_name);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| format!("asset '{asset_name}' not found in release"));

    let asset = match asset {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    println!("Downloading copad v{target_version}...");

    let response = match client().get(&asset.browser_download_url).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Download failed: {e}");
            std::process::exit(1);
        }
    };

    let bytes = match response.bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to read response: {e}");
            std::process::exit(1);
        }
    };

    let decoder = GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(decoder);

    let tmpdir = tempfile::tempdir().unwrap_or_else(|e| {
        eprintln!("Failed to create temp dir: {e}");
        std::process::exit(1);
    });

    if let Err(e) = archive.unpack(tmpdir.path()) {
        eprintln!("Failed to extract archive: {e}");
        std::process::exit(1);
    }

    let dest = install_dir();
    let binaries = ["copad", "coctl"];

    for name in &binaries {
        let src = tmpdir.path().join(name);
        if !src.exists() {
            eprintln!("Warning: {name} not found in archive, skipping.");
            continue;
        }

        let target = dest.join(name);
        let tmp_target = dest.join(format!(".{name}.new"));

        // Read source binary into memory
        let mut src_file = match fs::File::open(&src) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to open {name}: {e}");
                std::process::exit(1);
            }
        };

        let mut contents = Vec::new();
        if let Err(e) = src_file.read_to_end(&mut contents) {
            eprintln!("Failed to read {name}: {e}");
            std::process::exit(1);
        }

        // Write to temp file then atomic rename
        if let Err(e) = fs::write(&tmp_target, &contents) {
            eprintln!("Failed to write {name}: {e}");
            std::process::exit(1);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp_target, fs::Permissions::from_mode(0o755));
        }

        if let Err(e) = fs::rename(&tmp_target, &target) {
            eprintln!("Failed to install {name}: {e}");
            let _ = fs::remove_file(&tmp_target);
            std::process::exit(1);
        }
    }

    println!("copad v{target_version} installed successfully!");
}
