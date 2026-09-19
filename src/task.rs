use serde_json::Value;
use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const LABEL: &str = "com.marcell.MacWindowResizer";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
        usage();
        return;
    }
    let result = match args.as_slice() {
        [command] if matches!(command.as_str(), "enable" | "refresh" | "disable") => Paths::new()
            .and_then(|paths| match command.as_str() {
                "disable" => disable(&paths),
                "refresh" => {
                    disable(&paths)?;
                    enable(&paths)
                }
                _ => enable(&paths),
            }),
        _ => {
            usage();
            Err("expected enable, disable, or refresh".into())
        }
    };
    if let Err(error) = result {
        eprintln!("MacWindowResizer: {error}");
        std::process::exit(1);
    }
}

fn usage() {
    eprintln!("cargo enable   Build, install, start, and enable login startup.");
    eprintln!(
        "cargo disable  Uninstall the app and login service, reset permission, and remove logs."
    );
    eprintln!("cargo refresh  Run disable, then enable after a successful uninstall.");
}

struct Paths {
    project: PathBuf,
    app: PathBuf,
    agent: PathBuf,
    logs: PathBuf,
    domain: String,
}

impl Paths {
    fn new() -> Result<Self> {
        if !cfg!(target_os = "macos") {
            return Err("these commands require macOS".into());
        }
        let user_dir = PathBuf::from(env::var_os("HOME").ok_or("HOME is not set")?);
        Ok(Self {
            project: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            app: user_dir.join("Applications/MacWindowResizer.app"),
            agent: user_dir.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            logs: user_dir.join("Library/Logs/MacWindowResizer"),
            domain: format!("gui/{}", output(Command::new("id").arg("-u"))?.trim()),
        })
    }

    fn service(&self) -> String {
        format!("{}/{LABEL}", self.domain)
    }

    fn validate_app(&self) -> Result<()> {
        if self.app.exists() {
            let identifier = output(
                Command::new("plutil")
                    .args(["-extract", "CFBundleIdentifier", "raw"])
                    .arg(self.app.join("Contents/Info.plist")),
            )?;
            if identifier.trim() != LABEL {
                return Err(format!(
                    "refusing to replace or remove unrelated app: {}",
                    self.app.display()
                )
                .into());
            }
        }
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        if Command::new("launchctl")
            .args(["print", &self.service()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success()
        {
            run(Command::new("launchctl").args(["bootout", &self.service()]))?;
        }
        Ok(())
    }
}

fn enable(paths: &Paths) -> Result<()> {
    paths.validate_app()?;
    let bundle = build_bundle(&paths.project)?;
    fs::create_dir_all(paths.app.parent().unwrap())?;
    fs::create_dir_all(paths.agent.parent().unwrap())?;
    fs::create_dir_all(&paths.logs)?;
    let stage = Staging::new(paths.app.parent().unwrap())?;
    let staged_app = stage.0.join("MacWindowResizer.app");
    run(Command::new("/usr/bin/ditto").arg(bundle).arg(&staged_app))?;
    let agent = stage.0.join("agent.plist");
    let executable = xml(&paths
        .app
        .join("Contents/MacOS/MacWindowResizer")
        .to_string_lossy());
    let log = xml(&paths.logs.join("service.log").to_string_lossy());
    fs::write(
        &agent,
        format!(
            include_str!("../assets/LaunchAgent.plist"),
            LABEL = LABEL,
            executable = executable,
            log = log,
        ),
    )?;
    run(Command::new("plutil").arg("-lint").arg(&agent))?;
    paths.stop()?;
    remove_dir(&paths.app)?;
    fs::rename(staged_app, &paths.app)?;
    fs::rename(agent, &paths.agent)?;
    run(Command::new(LSREGISTER).arg("-f").arg(&paths.app))?;
    run(Command::new("launchctl").args(["enable", &paths.service()]))?;
    run(Command::new("launchctl")
        .args(["bootstrap", &paths.domain])
        .arg(&paths.agent))?;
    println!("Installed and started {}", paths.app.display());
    Ok(())
}

fn disable(paths: &Paths) -> Result<()> {
    paths.validate_app()?;
    paths.stop()?;
    run(Command::new("launchctl").args(["disable", &paths.service()]))?;
    if paths.app.exists() {
        if let Err(error) = run(Command::new("tccutil").args(["reset", "Accessibility", LABEL])) {
            eprintln!(
                "Could not reset Accessibility permission: {error}. Remove MacWindowResizer manually in System Settings."
            );
        }
        if let Err(error) = run(Command::new(LSREGISTER).arg("-u").arg(&paths.app)) {
            eprintln!("Could not unregister application: {error}");
        }
        remove_dir(&paths.app)?;
    }
    ignore_missing(fs::remove_file(&paths.agent))?;
    remove_dir(&paths.logs)?;
    println!("MacWindowResizer uninstalled. App, login service, and logs were removed.");
    Ok(())
}

fn build_bundle(project: &Path) -> Result<PathBuf> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let metadata: Value =
        serde_json::from_str(&output(Command::new(&cargo).current_dir(project).args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--locked",
        ]))?)?;
    let package = metadata["packages"]
        .as_array()
        .ok_or("missing packages")?
        .iter()
        .find(|package| package["name"] == env!("CARGO_PKG_NAME"))
        .ok_or("application package not found")?;
    let version = package["version"]
        .as_str()
        .ok_or("missing package version")?;
    let messages = output(Command::new(cargo).current_dir(project).args([
        "build",
        "--release",
        "--locked",
        "--bin",
        "MacWindowResizer",
        "--message-format=json-render-diagnostics",
    ]))?;
    let mut binary = None;
    for line in messages.lines() {
        let message: Value = serde_json::from_str(line)?;
        if message["reason"] == "compiler-artifact"
            && message["package_id"] == package["id"]
            && message["target"]["name"] == "MacWindowResizer"
        {
            binary = message["executable"].as_str().map(PathBuf::from);
        }
    }
    let binary = binary.ok_or("Cargo did not report the application executable")?;
    let dist = project.join("dist");
    fs::create_dir_all(&dist)?;
    let stage = Staging::new(&dist)?;
    let bundle = stage.0.join("MacWindowResizer.app");
    fs::create_dir_all(bundle.join("Contents/MacOS"))?;
    fs::create_dir_all(bundle.join("Contents/Resources"))?;
    fs::copy(binary, bundle.join("Contents/MacOS/MacWindowResizer"))?;
    for name in ["AppIcon.icns", "Assets.car"] {
        fs::copy(
            project.join("assets").join(name),
            bundle.join("Contents/Resources").join(name),
        )?;
    }
    let info = bundle.join("Contents/Info.plist");
    fs::copy(project.join("assets/Info.plist"), &info)?;
    for key in ["CFBundleVersion", "CFBundleShortVersionString"] {
        run(Command::new("plutil")
            .args(["-replace", key, "-string", version])
            .arg(&info))?;
    }
    let identity = env::var_os("SIGN_IDENTITY")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "-".into());
    run(Command::new("codesign")
        .args(["--force", "--sign"])
        .arg(&identity)
        .arg(&bundle))?;
    run(Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(&bundle))?;
    let destination = dist.join("MacWindowResizer.app");
    remove_dir(&destination)?;
    fs::rename(bundle, &destination)?;
    if identity == "-" {
        eprintln!("Ad-hoc signing: rebuilding may require granting Accessibility again.");
    }
    Ok(destination)
}

fn run(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?} failed: {status}").into());
    }
    Ok(())
}

fn output(command: &mut Command) -> Result<String> {
    let output = command.stderr(Stdio::inherit()).output()?;
    if !output.status.success() {
        return Err(format!("{command:?} failed: {}", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn remove_dir(path: &Path) -> io::Result<()> {
    ignore_missing(fs::remove_dir_all(path))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

struct Staging(PathBuf);

impl Staging {
    fn new(parent: &Path) -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = parent.join(format!(".MacWindowResizer-{}-{nonce}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
