//! `cargo xtask mac-bundle`: the Dev shell app with the brain nested in it as a background
//! helper, each with its own provisioning profile, signed with a passed-in Apple Development
//! identity, verified, and its brain launched once to show the system accepts its entitlements.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
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
/// The longest a codesign call may take, so one waiting on a keychain prompt nobody answers ends.
const SIGN_WAIT: Duration = Duration::from_secs(120);
/// More development certificates than any profile carries.
const MAX_CERTIFICATES: usize = 64;

pub(crate) const USAGE: &str =
    "  mac-bundle --identity <sha1> --shell-profile <file> --brain-profile <file> --out <new-dir>
                    build the Dev shell and brain, nest the brain in the shell app as a
                    background helper, embed each profile, sign both with that Apple
                    Development identity, verify them, and launch the brain once: it must
                    exit 69, not be killed; prints no team, identity, profile or path";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    identity: String,
    shell_profile: PathBuf,
    brain_profile: PathBuf,
    out: PathBuf,
}

/// What a profile grants, and the exact bytes that were checked, which are the ones embedded.
struct Profile {
    bytes: Vec<u8>,
    team: String,
    entitlements: Value,
    expires: String,
    platforms: Vec<String>,
    /// Uppercase SHA-1 hashes of the certificates it authorizes.
    certificates: Vec<String>,
}

/// One signed app: its bundle identifier, its executable and the profile that must authorize it.
struct Role<'a> {
    name: &'a str,
    id: &'a str,
    exe: &'a str,
    profile: Profile,
    background: bool,
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let args = parse(args)?;
    let ids = AppleIds::DEV;
    let read = |file: &Path, name| {
        let bytes = std::fs::read(file).with_context(|| format!("reading the {name} profile"))?;
        decode(bytes).with_context(|| format!("decoding the {name} profile"))
    };
    let shell = Role {
        name: "shell",
        id: ids.shell,
        exe: SHELL_EXE,
        profile: read(&args.shell_profile, "shell")?,
        background: false,
    };
    let brain = Role {
        name: "brain",
        id: ids.brain,
        exe: BRAIN_EXE,
        profile: read(&args.brain_profile, "brain")?,
        background: true,
    };
    let checked = |now: &str| {
        for role in [&shell, &brain] {
            check(
                &role.profile,
                role.id,
                ids.instance_group,
                &args.identity,
                now,
            )
            .with_context(|| format!("the {} profile", role.name))?;
        }
        ensure!(
            shell.profile.team == brain.profile.team,
            "the two profiles belong to different teams"
        );
        Ok(())
    };
    checked(&utc_now()?)?;
    let team = &shell.profile.team;

    std::fs::create_dir(&args.out).context("creating the output folder, which must not exist")?;
    let built = build()?;
    let shell_app = args
        .out
        .join(format!("{}.app", ProductIdentity::DEV.data_dir));
    let brain_app = shell_app.join(format!("Contents/Helpers/{BRAIN_EXE}.app"));
    for (role, app) in [(&shell, &shell_app), (&brain, &brain_app)] {
        stage(role, app, &built).with_context(|| format!("staging the {}", role.name))?;
    }
    // The build can be long, so a profile that expired meanwhile is refused before signing.
    checked(&utc_now()?)?;
    for (role, app) in [(&brain, &brain_app), (&shell, &shell_app)] {
        let file = args.out.join(format!("{}.entitlements", role.exe));
        std::fs::write(
            &file,
            plist(&entitlements(team, role.id, ids.instance_group))?,
        )?;
        sign(app, &file, &args.identity).with_context(|| format!("signing the {}", role.name))?;
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
            "the {}'s signed entitlements differ from the ones asked for",
            role.name
        );
    }
    let mut brain_run = Command::new(brain_app.join(format!("Contents/MacOS/{BRAIN_EXE}")));
    launch(&mut brain_run, LAUNCH_WAIT)?;
    println!("signed, verified and launched the bundle in the output folder");
    Ok(())
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args> {
    let (mut identity, mut shell, mut brain, mut out) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let (name, slot) = match flag.as_str() {
            "--identity" => ("--identity", &mut identity),
            "--shell-profile" => ("--shell-profile", &mut shell),
            "--brain-profile" => ("--brain-profile", &mut brain),
            "--out" => ("--out", &mut out),
            // An unknown argument could be a misplaced hash or path, so it isn't echoed.
            _ => bail!("mac-bundle doesn't take that argument"),
        };
        ensure!(slot.is_none(), "{name} given twice");
        *slot = Some(
            args.next()
                .with_context(|| format!("{name} needs a value"))?,
        );
    }
    let identity = identity.context("mac-bundle needs --identity")?;
    // Only an exact certificate hash, so codesign never picks an identity by name.
    ensure!(
        identity.len() == 40 && identity.bytes().all(|b| b.is_ascii_hexdigit()),
        "--identity is a 40-digit SHA-1 certificate hash"
    );
    Ok(Args {
        identity: identity.to_ascii_uppercase(),
        shell_profile: shell.context("mac-bundle needs --shell-profile")?.into(),
        brain_profile: brain.context("mac-bundle needs --brain-profile")?.into(),
        out: out.context("mac-bundle needs --out")?.into(),
    })
}

fn decode(bytes: Vec<u8>) -> Result<Profile> {
    let plist = pipe("/usr/bin/security", &["cms", "-D"], &bytes)?;
    let field = |key: &str, format: &str, expect: Option<&str>| {
        let mut args = vec!["-extract", key, format];
        if let Some(kind) = expect {
            args.extend(["-expect", kind]);
        }
        args.extend(["-o", "-", "-"]);
        pipe("/usr/bin/plutil", &args, &plist).with_context(|| format!("the profile's {key}"))
    };
    let teams: Vec<String> = serde_json::from_slice(&field("TeamIdentifier", "json", None)?)?;
    let [team] = <[String; 1]>::try_from(teams)
        .ok()
        .context("the profile names one team")?;
    let mut certificates = Vec::new();
    while certificates.len() < MAX_CERTIFICATES {
        let key = format!("DeveloperCertificates.{}", certificates.len());
        let Ok(encoded) = field(&key, "raw", Some("data")) else {
            break;
        };
        let der = pipe("/usr/bin/base64", &["-D"], &encoded)?;
        let hash = pipe("/usr/bin/shasum", &["-a", "1"], &der)?;
        let hash = String::from_utf8(hash)?;
        let hash = hash
            .split_whitespace()
            .next()
            .context("a certificate hash")?;
        certificates.push(hash.to_ascii_uppercase());
    }
    Ok(Profile {
        bytes,
        team,
        entitlements: serde_json::from_slice(&field("Entitlements", "json", None)?)?,
        expires: String::from_utf8(field("ExpirationDate", "raw", Some("date"))?)?
            .trim()
            .to_owned(),
        platforms: serde_json::from_slice(&field("Platform", "json", None)?)?,
        certificates,
    })
}

/// The profile is a current macOS one from a well-formed team, for exactly `id`, authorizing
/// `identity`'s certificate and allowing both of the role's keychain groups.
fn check(profile: &Profile, id: &str, group: &str, identity: &str, now: &str) -> Result<()> {
    let team = &profile.team;
    let text = |key| profile.entitlements.get(key).and_then(Value::as_str);
    ensure!(
        team.len() == 10
            && team
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
        "its team isn't a team identifier"
    );
    ensure!(
        profile.platforms.iter().any(|platform| platform == "OSX"),
        "it isn't a macOS profile"
    );
    ensure!(utc_seconds(&profile.expires), "its expiry isn't a UTC time");
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
    ensure!(
        profile.certificates.iter().any(|hash| hash == identity),
        "it doesn't authorize the signing certificate"
    );
    let allowed: Vec<&str> = profile
        .entitlements
        .get(GROUPS)
        .and_then(Value::as_array)
        .map(|groups| groups.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for wanted in [id, group] {
        let exact = format!("{team}.{wanted}");
        let any = format!("{team}.*");
        ensure!(
            allowed.iter().any(|&have| have == exact || have == any),
            "it doesn't allow the keychain group {wanted}"
        );
    }
    Ok(())
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the only form text order compares as time.
fn utc_seconds(text: &str) -> bool {
    let shape = b"0000-00-00T00:00:00Z";
    text.len() == shape.len()
        && text.bytes().zip(shape).all(|(byte, &want)| {
            if want == b'0' {
                byte.is_ascii_digit()
            } else {
                byte == want
            }
        })
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

/// The Dev shell and brain binaries Cargo builds now, by executable name: exactly one
/// `compiler-artifact` of each workspace package's binary target.
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
    artifacts(&output.stdout)
}

fn artifacts(stdout: &[u8]) -> Result<Vec<(String, PathBuf)>> {
    let mut built = Vec::new();
    for (package, exe) in [("kumochi-shell", SHELL_EXE), ("kumochi-brain", BRAIN_EXE)] {
        let mut found = stdout
            .split(|&byte| byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter(|message| {
                let target = &message["target"];
                message["reason"] == "compiler-artifact"
                    && message["package_id"]
                        .as_str()
                        .is_some_and(|id| id.contains(&format!("#{package}@")))
                    && target["name"] == exe
                    && target["kind"] == json!(["bin"])
            })
            .filter_map(|message| message["executable"].as_str().map(PathBuf::from));
        let path = found
            .next()
            .with_context(|| format!("Cargo built no {exe}"))?;
        ensure!(found.next().is_none(), "Cargo built more than one {exe}");
        built.push((exe.to_owned(), path));
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
    std::fs::write(
        contents.join("embedded.provisionprofile"),
        &role.profile.bytes,
    )?;
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

/// Runs `program` once, expecting exit 69 within `wait`. The brain refuses every entry with
/// 69; a profile or signature the system refused would kill it before `main`.
fn launch(program: &mut Command, wait: Duration) -> Result<()> {
    let mut child = program
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launching the signed brain")?;
    let status = wait_for(&mut child, wait)?
        .with_context(|| format!("the signed brain didn't exit within {wait:?}"))?;
    ensure!(
        status.code() == Some(UNAVAILABLE),
        "the signed brain ended with {status}, not {UNAVAILABLE}"
    );
    Ok(())
}

/// Waits up to `wait` for `child`. At the deadline, or when polling fails, it's stopped and
/// reaped; `None` means the deadline passed.
fn wait_for(child: &mut Child, wait: Duration) -> Result<Option<ExitStatus>> {
    let deadline = Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                end(child)?;
                return Ok(None);
            }
            Err(error) => {
                end(child)?;
                return Err(error.into());
            }
        }
    }
}

/// Stops and reaps `child`; a failed kill matters only while it still runs.
fn end(child: &mut Child) -> Result<ExitStatus> {
    if let Err(error) = child.kill()
        && child.try_wait()?.is_none()
    {
        return Err(error).context("stopping a child process");
    }
    child.wait().context("reaping a child process")
}

/// Runs `program` with `args` and then `path`, for at most [`SIGN_WAIT`]; its stdout. Its stderr
/// isn't shown, because it can name the team or the identity.
fn command(program: &str, args: &[&str], path: &Path) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    wait_for(&mut child, SIGN_WAIT)?
        .with_context(|| format!("{program} didn't finish within {SIGN_WAIT:?}"))?;
    finished(program, child.wait_with_output()?)
}

/// Runs `program` with `args` on `input`; its stdout, with its stderr withheld as above.
fn pipe(program: &str, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("the program's input")?
        .write_all(input)?;
    finished(program, child.wait_with_output()?)
}

fn finished(program: &str, output: std::process::Output) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "{program} failed ({}, {} bytes of stderr withheld)",
        output.status,
        output.stderr.len()
    );
    Ok(output.stdout)
}

fn to_json(plist: &[u8]) -> Result<Value> {
    let json = pipe(
        "/usr/bin/plutil",
        &["-convert", "json", "-o", "-", "-"],
        plist,
    )?;
    Ok(serde_json::from_slice(&json)?)
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
