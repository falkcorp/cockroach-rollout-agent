// file: src/main.rs
// version: 1.0.0
// guid: d16be11a-b10c-4d2e-853f-d4a1c0a3c617

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_BASE_URL: &str = "https://binaries.cockroachdb.com";
const DEFAULT_VERSION: &str = "latest";
const SUPPORTED_ARCHES: &[&str] = &["amd64", "arm64"];

#[derive(Debug)]
struct Config {
    base_url: String,
    version: String,
    artifacts_dir: PathBuf,
    service_name: String,
    binary_path: PathBuf,
    audit_log: PathBuf,
}

impl Config {
    fn from_env() -> Self {
        Self {
            base_url: env::var("CROACH_ROLLOUT_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            version: env::var("CROACH_ROLLOUT_VERSION")
                .unwrap_or_else(|_| DEFAULT_VERSION.to_string()),
            artifacts_dir: env::var_os("CROACH_ROLLOUT_ARTIFACTS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("dist")),
            service_name: env::var("CROACH_ROLLOUT_SERVICE")
                .unwrap_or_else(|_| "cockroachdb.service".to_string()),
            binary_path: env::var_os("CROACH_ROLLOUT_BINARY_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/usr/local/bin/cockroach")),
            audit_log: env::var_os("CROACH_ROLLOUT_AUDIT_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/log/cockroach-rollout-agent/audit.log")),
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());
    let config = Config::from_env();

    match command.as_str() {
        "fetch" => fetch_command(&config),
        "install" => {
            let artifact = args
                .next()
                .ok_or_else(|| "install requires a tarball path".to_string())?;
            install_command(&config, Path::new(&artifact))
        }
        "daemon" => daemon_command(&config),
        "self-check" => self_check_command(&config),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => Err(format!("unknown command: {command}")),
    }
}

fn fetch_command(config: &Config) -> Result<(), String> {
    fs::create_dir_all(&config.artifacts_dir).map_err(display_error)?;

    for arch in SUPPORTED_ARCHES {
        let url = cockroach_url(&config.base_url, &config.version, arch);
        let destination = config
            .artifacts_dir
            .join(format!("cockroach-{}.linux-{}.tgz", config.version, arch));

        audit(
            config,
            "fetch_start",
            &format!(
                "arch={arch} url={url} destination={}",
                destination.display()
            ),
        )?;
        run_command(
            "curl",
            [
                OsStr::new("--fail"),
                OsStr::new("--location"),
                OsStr::new("--show-error"),
                OsStr::new("--output"),
                destination.as_os_str(),
                OsStr::new(&url),
            ],
        )?;
        audit(
            config,
            "fetch_complete",
            &format!("arch={arch} destination={}", destination.display()),
        )?;
    }

    Ok(())
}

fn install_command(config: &Config, artifact: &Path) -> Result<(), String> {
    if !artifact.is_file() {
        return Err(format!("artifact does not exist: {}", artifact.display()));
    }

    let work_dir = env::temp_dir().join(format!("cockroach-rollout-{}", unix_time()?));
    let backup_path = config
        .binary_path
        .with_extension(format!("bak.{}", unix_time()?));

    fs::create_dir_all(&work_dir).map_err(display_error)?;
    audit(
        config,
        "install_start",
        &format!(
            "artifact={} binary={}",
            artifact.display(),
            config.binary_path.display()
        ),
    )?;

    run_command(
        "tar",
        [
            OsStr::new("-xzf"),
            artifact.as_os_str(),
            OsStr::new("-C"),
            work_dir.as_os_str(),
        ],
    )?;
    let extracted = find_cockroach_binary(&work_dir)?;

    run_command(
        "systemctl",
        [OsStr::new("stop"), OsStr::new(&config.service_name)],
    )?;
    fs::copy(&config.binary_path, &backup_path).map_err(display_error)?;
    fs::copy(&extracted, &config.binary_path).map_err(display_error)?;
    run_command(
        "chmod",
        [OsStr::new("0755"), config.binary_path.as_os_str()],
    )?;
    run_command(
        "systemctl",
        [OsStr::new("start"), OsStr::new(&config.service_name)],
    )?;

    audit(
        config,
        "install_complete",
        &format!(
            "backup={} binary={}",
            backup_path.display(),
            config.binary_path.display()
        ),
    )?;
    Ok(())
}

fn daemon_command(config: &Config) -> Result<(), String> {
    audit(
        config,
        "daemon_start",
        "daemon scaffold started; consensus transport is intentionally not enabled yet",
    )?;
    println!("daemon scaffold is installed");
    println!(
        "next implementation step: acquire CockroachDB-backed rollout lease before fetch/install"
    );
    println!("service={}", config.service_name);
    println!("binary={}", config.binary_path.display());
    Ok(())
}

fn self_check_command(config: &Config) -> Result<(), String> {
    audit(
        config,
        "self_check_start",
        "validating local permissions and commands",
    )?;
    require_command("curl")?;
    require_command("tar")?;
    require_command("systemctl")?;

    if !config.binary_path.exists() {
        return Err(format!(
            "binary path does not exist: {}",
            config.binary_path.display()
        ));
    }

    let parent = config.binary_path.parent().ok_or_else(|| {
        format!(
            "binary path has no parent: {}",
            config.binary_path.display()
        )
    })?;

    if !parent.is_dir() {
        return Err(format!(
            "binary parent is not a directory: {}",
            parent.display()
        ));
    }

    audit(config, "self_check_complete", "local validation completed")?;
    Ok(())
}

fn cockroach_url(base_url: &str, version: &str, arch: &str) -> String {
    if version == "latest" {
        format!("{base_url}/cockroach-latest.linux-{arch}.tgz")
    } else {
        format!("{base_url}/cockroach-{version}.linux-{arch}.tgz")
    }
}

fn find_cockroach_binary(root: &Path) -> Result<PathBuf, String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path).map_err(display_error)? {
            let entry = entry.map_err(display_error)?;
            let entry_path = entry.path();
            if entry_path.is_dir() {
                stack.push(entry_path);
            } else if entry_path.file_name() == Some(OsStr::new("cockroach")) {
                return Ok(entry_path);
            }
        }
    }
    Err(format!(
        "no cockroach binary found under {}",
        root.display()
    ))
}

fn require_command(name: &str) -> Result<(), String> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map_err(display_error)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("required command is unavailable: {name}"))
    }
}

fn run_command<I, S>(program: &str, args: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(display_error)?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with status {status}"))
    }
}

fn audit(config: &Config, event: &str, detail: &str) -> Result<(), String> {
    if let Some(parent) = config.audit_log.parent() {
        fs::create_dir_all(parent).map_err(display_error)?;
    }

    let line = format!(
        "ts={} event={} detail={}\n",
        unix_time()?,
        sanitize_log_field(event),
        sanitize_log_field(detail)
    );
    append_file(&config.audit_log, line.as_bytes()).map_err(display_error)
}

fn append_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(bytes)
}

fn sanitize_log_field(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

fn unix_time() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(display_error)
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn print_help() {
    println!("cockroach-rollout-agent");
    println!();
    println!("Commands:");
    println!("  fetch        Download latest CockroachDB Linux amd64 and arm64 tarballs");
    println!("  install PATH Stop systemd service, replace binary from tarball, restart service");
    println!("  daemon       Start daemon scaffold");
    println!("  self-check   Validate local commands and binary path");
    println!();
    println!("Environment:");
    println!("  CROACH_ROLLOUT_BASE_URL       Default: {DEFAULT_BASE_URL}");
    println!("  CROACH_ROLLOUT_VERSION        Default: {DEFAULT_VERSION}");
    println!("  CROACH_ROLLOUT_ARTIFACTS_DIR  Default: dist");
    println!("  CROACH_ROLLOUT_SERVICE        Default: cockroachdb.service");
    println!("  CROACH_ROLLOUT_BINARY_PATH    Default: /usr/local/bin/cockroach");
    println!("  CROACH_ROLLOUT_AUDIT_LOG      Default: /var/log/cockroach-rollout-agent/audit.log");
}
