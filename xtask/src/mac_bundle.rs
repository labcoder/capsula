//! `cargo xtask mac-bundle`: the Dev shell app with the brain nested in it as a background
//! helper, each with its own provisioning profile, signed with a passed-in Apple Development
//! identity, verified, and its brain launched once to show the system accepts its entitlements.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{AppleIds, BRAIN_EXE, ProductIdentity, SHELL_EXE};
use serde_json::{Value, json};

const APP_ID: &str = "com.apple.application-identifier";
const TEAM_ID: &str = "com.apple.developer.team-identifier";
const GROUPS: &str = "keychain-access-groups";
/// Every product entry exits with this until a signed brain can be admitted.
const UNAVAILABLE: i32 = 69;
const LAUNCH_WAIT: Duration = Duration::from_secs(10);

pub(crate) const USAGE: &str =
    "  mac-bundle --identity <sha1> --shell-profile <file> --brain-profile <file> --out <new-dir>
                    build the Dev shell and brain, nest the brain in the shell app as a
                    background helper, embed each profile, sign both with that Apple
                    Development identity, verify them, and launch the brain once: it must
                    exit 69, not be killed; prints no team, identity or profile value";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    identity: String,
    shell_profile: PathBuf,
    brain_profile: PathBuf,
    out: PathBuf,
}

/// What a decoded profile grants.
#[derive(Debug)]
struct Profile {
    team: String,
    entitlements: Value,
    expires: String,
    platforms: Vec<String>,
}

/// One signed app: its bundle identifier, executable and the profile that must authorize it.
struct Role<'a> {
    id: &'a str,
    exe: &'a str,
    profile: &'a Path,
    background: bool,
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let args = parse(args)?;
    let ids = AppleIds::DEV;
    let shell = Role {
        id: ids.shell,
        exe: SHELL_EXE,
        profile: &args.shell_profile,
        background: false,
    };
    let brain = Role {
        id: ids.brain,
        exe: BRAIN_EXE,
        profile: &args.brain_profile,
        background: true,
    };
    let now = utc_now()?;
    let shell_profile = decode(shell.profile)?;
    let brain_profile = decode(brain.profile)?;
    check(&shell_profile, shell.id, ids.instance_group, &now).context("the shell profile")?;
    check(&brain_profile, brain.id, ids.instance_group, &now).context("the brain profile")?;
    ensure!(
        shell_profile.team == brain_profile.team,
        "the two profiles belong to different teams"
    );
    let team = &shell_profile.team;

    std::fs::create_dir(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    let built = build()?;
    let shell_app = args
        .out
        .join(format!("{}.app", ProductIdentity::DEV.data_dir));
    let brain_app = shell_app.join(format!("Contents/Helpers/{BRAIN_EXE}.app"));
    for (role, app) in [(&shell, &shell_app), (&brain, &brain_app)] {
        stage(role, app, &built)?;
    }
    for (role, app) in [(&brain, &brain_app), (&shell, &shell_app)] {
        let file = args.out.join(format!("{}.entitlements", role.exe));
        std::fs::write(
            &file,
            plist(&entitlements(team, role.id, ids.instance_group))?,
        )?;
        sign(app, &file, &args.identity).with_context(|| format!("signing {}", role.exe))?;
    }
    command(
        "/usr/bin/codesign",
        &["--verify", "--strict", "--deep"],
        &shell_app,
    )
    .context("verifying the shell app")?;
    for (role, app) in [(&brain, &brain_app), (&shell, &shell_app)] {
        let shown = command(
            "/usr/bin/codesign",
            &["-d", "--xml", "--entitlements", "-"],
            app,
        )?;
        ensure!(
            to_json(&shown)? == entitlements(team, role.id, ids.instance_group),
            "{}'s signed entitlements differ from the ones asked for",
            role.exe
        );
    }
    launch(&brain_app.join(format!("Contents/MacOS/{BRAIN_EXE}")))?;
    println!("signed, verified and launched: {}", shell_app.display());
    Ok(())
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args> {
    let (mut identity, mut shell, mut brain, mut out) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--identity" => &mut identity,
            "--shell-profile" => &mut shell,
            "--brain-profile" => &mut brain,
            "--out" => &mut out,
            _ => bail!("mac-bundle doesn't take {flag:?}"),
        };
        ensure!(slot.is_none(), "{flag} given twice");
        *slot = Some(
            args.next()
                .with_context(|| format!("{flag} needs a value"))?,
        );
    }
    let identity = identity.context("mac-bundle needs --identity")?;
    // Only an exact certificate hash, so codesign never picks an identity by name.
    ensure!(
        identity.len() == 40 && identity.bytes().all(|b| b.is_ascii_hexdigit()),
        "--identity is a 40-digit SHA-1 certificate hash"
    );
    Ok(Args {
        identity,
        shell_profile: shell.context("mac-bundle needs --shell-profile")?.into(),
        brain_profile: brain.context("mac-bundle needs --brain-profile")?.into(),
        out: out.context("mac-bundle needs --out")?.into(),
    })
}

fn decode(file: &Path) -> Result<Profile> {
    let raw = command("/usr/bin/security", &["cms", "-D", "-i"], file)
        .with_context(|| format!("decoding {}", file.display()))?;
    let field = |key: &str, format: &str| {
        let mut child = Command::new("/usr/bin/plutil")
            .args(["-extract", key, format, "-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .context("plutil's input")?
            .write_all(&raw)?;
        let output = child.wait_with_output()?;
        ensure!(output.status.success(), "the profile has no {key}");
        Ok::<_, anyhow::Error>(output.stdout)
    };
    let teams: Vec<String> = serde_json::from_slice(&field("TeamIdentifier", "json")?)?;
    let [team] = <[String; 1]>::try_from(teams)
        .ok()
        .context("the profile names one team")?;
    Ok(Profile {
        team,
        entitlements: serde_json::from_slice(&field("Entitlements", "json")?)?,
        expires: String::from_utf8(field("ExpirationDate", "raw")?)?
            .trim()
            .to_owned(),
        platforms: serde_json::from_slice(&field("Platform", "json")?)?,
    })
}

/// The profile is a current macOS one for exactly `id` and allows the instance group.
fn check(profile: &Profile, id: &str, group: &str, now: &str) -> Result<()> {
    let team = &profile.team;
    let text = |key| profile.entitlements.get(key).and_then(Value::as_str);
    ensure!(
        profile.platforms.iter().any(|platform| platform == "OSX"),
        "it isn't a macOS profile"
    );
    // Both are ISO 8601 UTC with seconds, so text order is time order.
    ensure!(profile.expires.as_str() > now, "it has expired");
    ensure!(
        text(TEAM_ID) == Some(team.as_str()),
        "its team doesn't match its entitlements"
    );
    ensure!(
        text(APP_ID) == Some(format!("{team}.{id}").as_str()),
        "it isn't for exactly {id}"
    );
    let allowed: Vec<&str> = profile
        .entitlements
        .get(GROUPS)
        .and_then(Value::as_array)
        .map(|groups| groups.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for wanted in [id, group] {
        let wanted = format!("{team}.{wanted}");
        ensure!(
            allowed
                .iter()
                .any(|&have| have == wanted || have == format!("{team}.*")),
            "it doesn't allow the keychain group {}",
            wanted.trim_start_matches(team.as_str())
        );
    }
    Ok(())
}

/// The entitlements a role is signed with: its own App ID, which is also its private keychain
/// group, and the instance group it shares.
fn entitlements(team: &str, id: &str, group: &str) -> Value {
    json!({
        APP_ID: format!("{team}.{id}"),
        TEAM_ID: team,
        GROUPS: [format!("{team}.{id}"), format!("{team}.{group}")],
    })
}

fn info(role: &Role<'_>) -> Value {
    let mut info = json!({
        "CFBundleIdentifier": role.id,
        "CFBundleExecutable": role.exe,
        "CFBundlePackageType": "APPL",
        "CFBundleVersion": env!("CARGO_PKG_VERSION"),
        "CFBundleShortVersionString": env!("CARGO_PKG_VERSION"),
    });
    if role.background {
        info["LSBackgroundOnly"] = Value::Bool(true);
    }
    info
}

/// A property list of strings, booleans and arrays of strings, all the bundle needs.
fn plist(value: &Value) -> Result<String> {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn write(out: &mut String, value: &Value) -> Result<()> {
        match value {
            Value::String(text) => {
                out.push_str("<string>");
                out.push_str(&escape(text));
                out.push_str("</string>");
            }
            Value::Bool(true) => out.push_str("<true/>"),
            Value::Bool(false) => out.push_str("<false/>"),
            Value::Array(items) => {
                out.push_str("<array>");
                for item in items {
                    write(out, item)?;
                }
                out.push_str("</array>");
            }
            Value::Object(map) => {
                out.push_str("<dict>");
                for (key, item) in map {
                    out.push_str("<key>");
                    out.push_str(&escape(key));
                    out.push_str("</key>");
                    write(out, item)?;
                }
                out.push_str("</dict>");
            }
            Value::Null | Value::Number(_) => {
                bail!("a bundle property list holds no number or null")
            }
        }
        Ok(())
    }
    let mut out = String::from(concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        "\n",
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#,
        "\n",
        r#"<plist version="1.0">"#
    ));
    write(&mut out, value)?;
    out.push_str("</plist>\n");
    Ok(out)
}

/// The Dev shell and brain executables, built by Cargo now.
fn build() -> Result<Vec<(String, PathBuf)>> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "build",
            "--locked",
            "--message-format=json-render-diagnostics",
        ])
        .args(["-p", "kumochi-shell", "-p", "kumochi-brain"])
        .stderr(Stdio::inherit())
        .output()?;
    ensure!(output.status.success(), "the Dev build failed");
    let mut built = Vec::new();
    for line in output.stdout.split(|&byte| byte == b'\n') {
        let Ok(message) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let (Some(name), Some(exe)) = (
            message["target"]["name"].as_str(),
            message["executable"].as_str(),
        ) {
            built.push((name.to_owned(), PathBuf::from(exe)));
        }
    }
    Ok(built)
}

fn stage(role: &Role<'_>, app: &Path, built: &[(String, PathBuf)]) -> Result<()> {
    let exe = built
        .iter()
        .find(|(name, _)| name == role.exe)
        .map(|(_, path)| path)
        .with_context(|| format!("Cargo built no {}", role.exe))?;
    let contents = app.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::copy(exe, contents.join("MacOS").join(role.exe))?;
    std::fs::copy(role.profile, contents.join("embedded.provisionprofile"))?;
    std::fs::write(contents.join("Info.plist"), plist(&info(role))?)?;
    Ok(())
}

fn sign(app: &Path, entitlements: &Path, identity: &str) -> Result<()> {
    let entitlements = entitlements
        .to_str()
        .context("an entitlements path that is text")?;
    command(
        "/usr/bin/codesign",
        &[
            "--force",
            "--timestamp=none",
            "--options",
            "runtime",
            "--entitlements",
            entitlements,
            "--sign",
            identity,
        ],
        app,
    )
    .map(drop)
}

/// The brain refuses every entry with 69; a profile or signature the system refused would kill
/// it before `main`.
fn launch(exe: &Path) -> Result<()> {
    let mut child = Command::new(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launching the signed brain")?;
    let deadline = Instant::now() + LAUNCH_WAIT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            bail!("the signed brain didn't exit within {LAUNCH_WAIT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    ensure!(
        status.code() == Some(UNAVAILABLE),
        "the signed brain ended with {status}, not {UNAVAILABLE}"
    );
    Ok(())
}

/// Runs `program` with `args` and then `path`; its stdout. Its stderr isn't shown, because it
/// can name the team or the identity.
fn command(program: &str, args: &[&str], path: &Path) -> Result<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    ensure!(
        output.status.success(),
        "{program} failed ({}, {} bytes of stderr withheld)",
        output.status,
        output.stderr.len()
    );
    Ok(output.stdout)
}

fn to_json(plist: &[u8]) -> Result<Value> {
    let mut child = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .context("plutil's input")?
        .write_all(plist)?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "the signed entitlements aren't a property list"
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn utc_now() -> Result<String> {
    let output = Command::new("/bin/date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()?;
    ensure!(output.status.success(), "reading the time failed");
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests;
