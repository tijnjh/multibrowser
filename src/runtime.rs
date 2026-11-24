//! Self-managed engine runtimes. Nothing needs to be installed on the machine:
//! on first use we download pinned, checksum-verified builds into the app's data
//! directory and reuse them afterwards.
//!
//! - Node.js (official build from nodejs.org) + `playwright-core` (from npm) host the
//!   Playwright sidecar, which in turn downloads Playwright's Chromium, Firefox and
//!   WebKit builds.
//! - Ladybird comes from the official release channel (macOS .dmg, or the .deb from
//!   Ladybird's apt repository on Linux, unpacked locally without installing it).

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use sha2::{Digest, Sha256, Sha512};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const NODE_VERSION: &str = "v26.13.1";
pub const PLAYWRIGHT_VERSION: &str = "1.65.1";

/// Status callback: receives short human-readable progress messages.
pub type Report<'a> = &'a dyn Fn(String);

static NODE_LOCK: Mutex<()> = Mutex::new(());
static BROWSER_INSTALL_LOCK: Mutex<()> = Mutex::new(());
static LADYBIRD_LOCK: Mutex<()> = Mutex::new(());

pub fn data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("MB_HOME") {
        return PathBuf::from(p);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/multibrowser")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("multibrowser")
    }
}

fn os_arch() -> Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!("unsupported OS: {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => bail!("unsupported CPU architecture: {other}"),
    };
    Ok((os, arch))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .user_agent("multibrowser-runtime/0.1")
        .build()
}

fn get_text(url: &str) -> Result<String> {
    Ok(agent().get(url).call().with_context(|| format!("GET {url}"))?.into_string()?)
}

enum Checksum {
    Sha256Hex(String),
    /// npm-style SRI: base64 sha512.
    Sha512B64(String),
}

/// Stream `url` to `dest`, verifying the checksum and reporting progress.
fn download(url: &str, dest: &Path, sum: Checksum, label: &str, report: Report) -> Result<()> {
    let resp = agent().get(url).call().with_context(|| format!("GET {url}"))?;
    let total: Option<u64> = resp.header("Content-Length").and_then(|v| v.parse().ok());
    let mut reader = resp.into_reader();
    let tmp = dest.with_extension("part");
    let mut out = fs::File::create(&tmp)?;
    let mut h256 = Sha256::new();
    let mut h512 = Sha512::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    let mut last = Instant::now() - Duration::from_secs(1);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        match sum {
            Checksum::Sha256Hex(_) => h256.update(&buf[..n]),
            Checksum::Sha512B64(_) => h512.update(&buf[..n]),
        }
        done += n as u64;
        if last.elapsed() > Duration::from_millis(250) {
            last = Instant::now();
            report(match total {
                Some(t) => format!("Downloading {label}… {}% of {} MB", done * 100 / t.max(1), t / 1_048_576),
                None => format!("Downloading {label}… {} MB", done / 1_048_576),
            });
        }
    }
    out.flush()?;
    drop(out);
    let ok = match &sum {
        Checksum::Sha256Hex(want) => hex(&h256.finalize()).eq_ignore_ascii_case(want),
        Checksum::Sha512B64(want) => {
            base64::engine::general_purpose::STANDARD.encode(h512.finalize()) == *want
        }
    };
    if !ok {
        let _ = fs::remove_file(&tmp);
        bail!("checksum mismatch for {url}");
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Unpack a tar stream into `dest`, dropping the first `strip` path components.
fn untar(reader: impl Read, dest: &Path, strip: usize) -> Result<()> {
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let rel: PathBuf = path.components().skip(strip).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&out).with_context(|| format!("unpacking {}", rel.display()))?;
    }
    Ok(())
}

/// Run `f` to populate a fresh temporary directory, then atomically move it to `final_dir`.
fn install_dir(final_dir: &Path, f: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let tmp = final_dir.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    if let Err(e) = f(&tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let _ = fs::remove_dir_all(final_dir);
    fs::rename(&tmp, final_dir)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Node.js + playwright-core

pub struct PlaywrightRuntime {
    pub node: PathBuf,
    pub package: PathBuf,
    pub browsers: PathBuf,
}

pub fn ensure_playwright(report: Report) -> Result<PlaywrightRuntime> {
    let _guard = NODE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = data_dir();
    fs::create_dir_all(root.join("downloads"))?;

    // Node.js
    let (os, arch) = os_arch()?;
    let node_dir = root.join(format!("node-{NODE_VERSION}"));
    let node = node_dir.join("bin/node");
    if !node.exists() {
        report("Fetching Node.js checksums…".into());
        let name = format!("node-{NODE_VERSION}-{os}-{arch}.tar.gz");
        let base = format!("https://nodejs.org/dist/{NODE_VERSION}");
        let sums = get_text(&format!("{base}/SHASUMS256.txt"))?;
        let sha = sums
            .lines()
            .find_map(|l| l.strip_suffix(&format!("  {name}")))
            .ok_or_else(|| anyhow!("{name} not listed in SHASUMS256.txt"))?
            .trim()
            .to_string();
        let archive = root.join("downloads").join(&name);
        download(&format!("{base}/{name}"), &archive, Checksum::Sha256Hex(sha), "Node.js", report)?;
        report("Unpacking Node.js…".into());
        install_dir(&node_dir, |tmp| {
            untar(flate2::read::GzDecoder::new(fs::File::open(&archive)?), tmp, 1)
        })?;
        let _ = fs::remove_file(&archive);
    }

    // playwright-core
    let pkg_dir = root.join(format!("playwright-core-{PLAYWRIGHT_VERSION}"));
    if !pkg_dir.join("package.json").exists() {
        report("Fetching Playwright metadata…".into());
        let meta: serde_json::Value = serde_json::from_str(&get_text(&format!(
            "https://registry.npmjs.org/playwright-core/{PLAYWRIGHT_VERSION}"
        ))?)?;
        let tarball = meta["dist"]["tarball"].as_str().context("npm metadata: no tarball")?;
        let integrity = meta["dist"]["integrity"].as_str().context("npm metadata: no integrity")?;
        let sha512 = integrity.strip_prefix("sha512-").context("unexpected integrity format")?;
        let archive = root.join("downloads/playwright-core.tgz");
        download(tarball, &archive, Checksum::Sha512B64(sha512.into()), "Playwright", report)?;
        report("Unpacking Playwright…".into());
        install_dir(&pkg_dir, |tmp| {
            untar(flate2::read::GzDecoder::new(fs::File::open(&archive)?), tmp, 1)
        })?;
        let _ = fs::remove_file(&archive);
    }

    Ok(PlaywrightRuntime { node, package: pkg_dir, browsers: root.join("browsers") })
}

/// Download Playwright's build of `engine` ("chromium" | "firefox" | "webkit") if needed.
pub fn ensure_playwright_browser(rt: &PlaywrightRuntime, engine: &str, report: Report) -> Result<()> {
    let marker = rt.browsers.join(format!(".multibrowser-{engine}-{PLAYWRIGHT_VERSION}"));
    if marker.exists() {
        return Ok(());
    }
    // Playwright serialises installs with its own lock; we also avoid interleaved progress.
    report("Waiting for other downloads…".into());
    let _guard = BROWSER_INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    report(format!("Downloading {engine}…"));
    fs::create_dir_all(&rt.browsers)?;
    let mut cmd = Command::new(&rt.node);
    cmd.arg(rt.package.join("cli.js")).arg("install");
    if engine == "chromium" {
        cmd.arg("--only-shell"); // the headless build is all we need
    }
    cmd.arg(engine)
        .env("PLAYWRIGHT_BROWSERS_PATH", &rt.browsers)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running playwright install")?;
    let stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut s);
        s
    });
    // Progress lines look like "|■■■■■     |  40% of 92.1 MiB".
    let mut what = engine.to_string();
    for line in BufReader::new(child.stdout.take().unwrap()).split(b'\n').map_while(Result::ok) {
        let line = String::from_utf8_lossy(&line);
        for part in line.split('\r') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("Downloading ") {
                what = rest.split(" from ").next().unwrap_or(rest).to_string();
            } else if let Some(pct) = part.rsplit('|').next().filter(|s| s.contains('%')) {
                report(format!("Downloading {what}… {}", pct.trim()));
            }
        }
    }
    let status = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        bail!("playwright install {engine} failed: {}", err.lines().last().unwrap_or("").trim());
    }
    fs::write(marker, b"")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Ladybird

/// Returns the path to Ladybird's `WebDriver` binary, downloading Ladybird if needed.
pub fn ensure_ladybird(report: Report) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("MB_LADYBIRD_WEBDRIVER") {
        return Ok(PathBuf::from(p));
    }
    let _guard = LADYBIRD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let current = ladybird_dir();
    if let Some(wd) = find_ladybird_webdriver(&current) {
        return Ok(wd);
    }
    install_ladybird(&latest_ladybird(report)?, report)?;
    find_ladybird_webdriver(&current).ok_or_else(|| anyhow!("WebDriver not found in Ladybird package"))
}

fn ladybird_dir() -> PathBuf {
    data_dir().join("ladybird/current")
}

/// The Ladybird version in the data folder, if one is installed.
fn installed_ladybird_version() -> Option<String> {
    let dir = ladybird_dir();
    find_ladybird_webdriver(&dir)?;
    Some(fs::read_to_string(dir.join("VERSION")).ok()?.trim().to_string())
}

fn find_ladybird_webdriver(dir: &Path) -> Option<PathBuf> {
    let mac = dir.join("Ladybird.app/Contents/MacOS/WebDriver");
    if mac.exists() {
        return Some(mac);
    }
    walk(dir).into_iter().find(|p| {
        p.file_name().is_some_and(|n| n == "WebDriver") && p.is_file()
    })
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                _ => out.push(p),
            }
        }
    }
    out
}

/// A downloadable Ladybird build for this machine: a .dmg on macOS, a .deb on Linux.
struct LadybirdRelease {
    version: String,
    url: String,
    sha256: String,
}

fn latest_ladybird(report: Report) -> Result<LadybirdRelease> {
    if cfg!(target_os = "macos") { latest_ladybird_macos(report) } else { latest_ladybird_linux(report) }
}

fn latest_ladybird_macos(report: Report) -> Result<LadybirdRelease> {
    report("Fetching Ladybird releases…".into());
    let releases: serde_json::Value =
        serde_json::from_str(&get_text("https://download.ladybird.org/releases.json")?)?;
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        a => a,
    };
    releases["releases"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|r| {
            let d = r["downloads"].as_array()?.iter().find(|d| {
                d["platform"] == "macOS" && d["architecture"] == arch
            })?;
            Some(LadybirdRelease {
                version: r["version"].as_str()?.to_string(),
                url: d["url"].as_str()?.to_string(),
                sha256: d["sha256"].as_str()?.to_string(),
            })
        })
        .ok_or_else(|| anyhow!("no Ladybird release for macOS {arch}"))
}

fn latest_ladybird_linux(report: Report) -> Result<LadybirdRelease> {
    // Ladybird publishes Linux builds through its apt repository; we take the Ubuntu 24.04
    // build (oldest glibc, so it runs on the most distros) and unpack the .deb ourselves.
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => bail!("no Ladybird build for {a}"),
    };
    report("Fetching Ladybird package index…".into());
    let base = "https://apt.ladybird.org";
    let index = get_text(&format!("{base}/dists/noble/main/binary-{arch}/Packages"))?;
    let mut best: Option<(String, String, String)> = None; // (version, filename, sha256)
    for para in index.split("\n\n") {
        let field = |k: &str| {
            para.lines().find_map(|l| l.strip_prefix(&format!("{k}: ")).map(str::to_string))
        };
        if field("Package").as_deref() != Some("ladybird") {
            continue;
        }
        if let (Some(v), Some(f), Some(s)) = (field("Version"), field("Filename"), field("SHA256"))
            && best.as_ref().is_none_or(|(bv, _, _)| v > *bv)
        {
            best = Some((v, f, s));
        }
    }
    let (version, filename, sha256) = best.ok_or_else(|| anyhow!("ladybird not found in apt index"))?;
    Ok(LadybirdRelease { version, url: format!("{base}/{filename}"), sha256 })
}

/// Download `rel` and unpack it over `ladybird/current`. The old install stays in place
/// until the new one has been downloaded, verified and unpacked.
fn install_ladybird(rel: &LadybirdRelease, report: Report) -> Result<()> {
    fs::create_dir_all(data_dir().join("downloads"))?;
    let ext = if cfg!(target_os = "macos") { "dmg" } else { "deb" };
    let pkg = data_dir().join(format!("downloads/ladybird-{}.{ext}", rel.version));
    let label = format!("Ladybird {}", rel.version);
    download(&rel.url, &pkg, Checksum::Sha256Hex(rel.sha256.clone()), &label, report)?;
    report("Unpacking Ladybird…".into());
    let result = install_dir(&ladybird_dir(), |tmp| {
        if cfg!(target_os = "macos") { unpack_dmg(&pkg, tmp)? } else { unpack_deb(&pkg, tmp)? }
        fs::write(tmp.join("VERSION"), &rel.version)?;
        Ok(())
    });
    let _ = fs::remove_file(&pkg);
    result
}

fn unpack_dmg(dmg: &Path, dest: &Path) -> Result<()> {
    let mnt = data_dir().join(format!("downloads/ladybird-mnt-{}", std::process::id()));
    fs::create_dir_all(&mnt)?;
    let st = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet", "-mountpoint"])
        .arg(&mnt)
        .arg(dmg)
        .status()?;
    if !st.success() {
        bail!("hdiutil attach failed");
    }
    let st = Command::new("ditto").arg(mnt.join("Ladybird.app")).arg(dest.join("Ladybird.app")).status();
    let _ = Command::new("hdiutil").args(["detach", "-quiet"]).arg(&mnt).status();
    let _ = fs::remove_dir(&mnt);
    if !st?.success() {
        bail!("copying Ladybird.app failed");
    }
    Ok(())
}

fn unpack_deb(deb: &Path, dest: &Path) -> Result<()> {
    let data = fs::read(deb)?;
    let (name, payload) = ar_member(&data, "data.tar")?;
    let cursor = std::io::Cursor::new(payload);
    if name.ends_with(".zst") {
        untar(zstd::Decoder::new(cursor)?, dest, 0)
    } else if name.ends_with(".xz") {
        untar(xz2::read::XzDecoder::new(cursor), dest, 0)
    } else if name.ends_with(".gz") {
        untar(flate2::read::GzDecoder::new(cursor), dest, 0)
    } else {
        untar(cursor, dest, 0)
    }
}

/// Minimal `ar` reader (the .deb container format): find the member starting with `prefix`.
fn ar_member<'a>(data: &'a [u8], prefix: &str) -> Result<(String, &'a [u8])> {
    let mut pos = data.strip_prefix(b"!<arch>\n").map(|_| 8).context("not an ar archive")?;
    while pos + 60 <= data.len() {
        let hdr = &data[pos..pos + 60];
        let name = String::from_utf8_lossy(&hdr[0..16]).trim().trim_end_matches('/').to_string();
        let size: usize = String::from_utf8_lossy(&hdr[48..58]).trim().parse()?;
        let start = pos + 60;
        let end = start + size;
        if end > data.len() {
            bail!("truncated ar archive");
        }
        if name.starts_with(prefix) {
            return Ok((name, &data[start..end]));
        }
        pos = end + (size & 1);
    }
    bail!("{prefix}* not found in .deb")
}

/// Extra environment for running the unpacked Linux Ladybird (its private libraries).
pub fn ladybird_env(webdriver: &Path) -> Vec<(String, String)> {
    if cfg!(target_os = "macos") {
        return Vec::new();
    }
    // .../usr/bin/WebDriver or .../usr/libexec/ladybird/WebDriver -> .../usr
    let usr = webdriver.ancestors().find(|p| p.ends_with("usr")).map(Path::to_path_buf);
    let Some(usr) = usr else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = walk(&usr)
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(".so")))
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    let mut ld: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
        ld.push(existing);
    }
    vec![
        ("LD_LIBRARY_PATH".into(), ld.join(":")),
        ("PATH".into(), format!("{}:{}", usr.join("bin").display(), std::env::var("PATH").unwrap_or_default())),
    ]
}

// ---------------------------------------------------------------------------------------
// `multibrowser --update`

/// Brings the data folder up to date: Ladybird to its latest release, and Node.js,
/// `playwright-core` and Playwright's browsers to the versions pinned in this build.
/// Versions that are no longer pinned are deleted. With `check_only`, only reports.
pub fn update(check_only: bool) -> Result<()> {
    let root = data_dir();
    println!("Data folder: {}", root.display());
    let report = |m: String| {
        eprint!("\r\x1b[2K  {m}");
        let _ = std::io::stderr().flush();
    };
    let clear = || eprint!("\r\x1b[2K");

    // Ladybird follows its release channel, so this is where new versions come from.
    if let Some(p) = std::env::var_os("MB_LADYBIRD_WEBDRIVER") {
        println!("Ladybird: using {} (MB_LADYBIRD_WEBDRIVER), skipped", Path::new(&p).display());
    } else {
        let latest = latest_ladybird(&report)?;
        clear();
        match installed_ladybird_version() {
            Some(v) if v == latest.version => println!("Ladybird {v}: up to date"),
            installed => {
                let from = installed.unwrap_or_else(|| "not installed".into());
                println!("Ladybird: {from} → {}", latest.version);
                if !check_only {
                    let _guard = LADYBIRD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                    install_ladybird(&latest, &report)?;
                    clear();
                }
            }
        }
    }

    let node = root.join(format!("node-{NODE_VERSION}/bin/node"));
    let pkg = root.join(format!("playwright-core-{PLAYWRIGHT_VERSION}/package.json"));
    let state = |ok: bool| if ok { "installed" } else { "to download" };
    println!("Node.js {NODE_VERSION}: {}", state(node.exists()));
    println!("playwright-core {PLAYWRIGHT_VERSION}: {}", state(pkg.exists()));
    if check_only {
        for engine in ["chromium", "firefox", "webkit"] {
            let marker = root.join(format!("browsers/.multibrowser-{engine}-{PLAYWRIGHT_VERSION}"));
            println!("Playwright {engine}: {}", state(marker.exists()));
        }
        let stale = stale_entries(&root);
        if !stale.is_empty() {
            println!("Old versions to delete:");
            for p in stale {
                println!("  {}", p.display());
            }
        }
        return Ok(());
    }

    // Delete old Node/Playwright versions first: Playwright's installer then garbage-collects
    // the browser builds that only the deleted playwright-core versions used.
    for p in stale_entries(&root) {
        println!("Deleting {}", p.display());
        let r = if p.is_dir() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
        r.with_context(|| format!("deleting {}", p.display()))?;
    }
    let rt = ensure_playwright(&report)?;
    for engine in ["chromium", "firefox", "webkit"] {
        ensure_playwright_browser(&rt, engine, &report)?;
    }
    clear();
    // Product versions of the browsers this playwright-core uses.
    if let Ok(text) = fs::read_to_string(rt.package.join("browsers.json"))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        for b in v["browsers"].as_array().into_iter().flatten() {
            if let (Some(name @ ("chromium" | "firefox" | "webkit")), Some(ver)) =
                (b["name"].as_str(), b["browserVersion"].as_str())
            {
                println!("Playwright {name} {ver}: installed");
            }
        }
    }
    Ok(())
}

/// Files and folders in the data folder that belong to versions this build doesn't use,
/// plus leftovers from interrupted installs.
fn stale_entries(root: &Path) -> Vec<PathBuf> {
    let keep = [
        format!("node-{NODE_VERSION}"),
        format!("playwright-core-{PLAYWRIGHT_VERSION}"),
        format!("sidecar-{PLAYWRIGHT_VERSION}.js"),
    ];
    let mut out = Vec::new();
    let mut scan = |dir: &Path, is_stale: &dyn Fn(&str) -> bool| {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_stale(&name) {
                out.push(e.path());
            }
        }
    };
    scan(root, &|n| {
        let versioned = n.starts_with("node-") || n.starts_with("playwright-core-") || n.starts_with("sidecar-");
        (versioned && !keep.iter().any(|k| k == n)) || n.contains(".tmp-")
    });
    scan(&root.join("browsers"), &|n| {
        n.starts_with(".multibrowser-") && !n.ends_with(&format!("-{PLAYWRIGHT_VERSION}"))
    });
    scan(&root.join("ladybird"), &|n| n.contains(".tmp-"));
    scan(&root.join("downloads"), &|_| true);
    out.sort();
    out
}
