//! `cargo xtask mac-bundle`: the Dev shell app with the brain nested in it as a background
//! helper, each with its own provisioning profile, signed with a passed-in Apple Development
//! identity (or, with `--beta`, a Developer ID one), checked as shipped, and its brain launched
//! once to show the system accepts its entitlements.

use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{AppleIds, BRAIN_EXE, ProductIdentity, SHELL_EXE};
use serde_json::{Value, json};

const APP_ID: &str = "com.apple.application-identifier";
const TEAM_ID: &str = "com.apple.developer.team-identifier";
const GROUPS: &str = "keychain-access-groups";
/// The debugger entitlement, which no bundle this task builds may carry (K-8).
const GET_TASK_ALLOW: &str = "com.apple.security.get-task-allow";
/// The code directory flag for the hardened runtime (`CS_RUNTIME`).
const RUNTIME: u32 = 0x1_0000;
/// Every product entry exits with this until a signed brain can be admitted.
const UNAVAILABLE: i32 = 69;
const LAUNCH_WAIT: Duration = Duration::from_secs(10);
/// The longest a codesign call may take, so one waiting on a keychain prompt nobody answers ends.
const SIGN_WAIT: Duration = Duration::from_secs(120);
/// The longest a profile or plist tool may take.
const TOOL_WAIT: Duration = Duration::from_secs(30);
/// How long a stopped tool's pipes get to close; one a helper outside its group keeps open fails
/// the task instead.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// More development certificates than any profile carries.
const MAX_CERTIFICATES: usize = 64;

pub(crate) const USAGE: &str =
    "  mac-bundle [--beta] --identity <sha1> --shell-profile <file> --brain-profile <file> --out <new-dir>
                    build the Dev shell and brain, nest the brain in the shell app as a
                    background helper, embed each profile, sign both with that Apple
                    Development identity (--beta: Developer ID, with Developer ID profiles),
                    check the signed apps carry no get-task-allow, the hardened runtime and
                    their embedded profiles' App ID prefix and groups, and launch the brain
                    once: it must exit 69, not be killed; prints no team, identity, profile or path";

/// Which identity and profiles sign the bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Apple Development, with development profiles.
    Dev,
    /// Developer ID, with Developer ID profiles, for beta artifacts.
    Beta,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: Mode,
    identity: String,
    shell_profile: PathBuf,
    brain_profile: PathBuf,
    out: PathBuf,
}

/// What a profile grants, and the exact bytes that were checked, which are the ones embedded.
struct Profile {
    bytes: Vec<u8>,
    team: String,
    /// The App ID prefix, which keychain groups and the App ID start with; usually, but not
    /// always, the team.
    prefix: String,
    /// A Developer ID profile's mark: `ProvisionsAllDevices` is exactly true.
    all_devices: bool,
    /// How many devices a development profile lists; `None` when it lists none.
    devices: Option<usize>,
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
                args.mode,
            )
            .with_context(|| format!("the {} profile", role.name))?;
        }
        ensure!(
            shell.profile.team == brain.profile.team,
            "the two profiles belong to different teams"
        );
        ensure!(
            shell.profile.prefix == brain.profile.prefix,
            "the two profiles have different App ID prefixes"
        );
        Ok(())
    };
    checked(&utc_now()?)?;
    let (team, prefix) = (&shell.profile.team, &shell.profile.prefix);

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
            plist(&entitlements(prefix, team, role.id, ids.instance_group))?,
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
        audit(app, role.id, ids.instance_group)
            .with_context(|| format!("the signed {}", role.name))?;
    }
    let mut brain_run = Command::new(brain_app.join(format!("Contents/MacOS/{BRAIN_EXE}")));
    launch(&mut brain_run, LAUNCH_WAIT)?;
    println!("signed, verified and launched the bundle in the output folder");
    Ok(())
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args> {
    let (mut identity, mut shell, mut brain, mut out) = (None, None, None, None);
    let mut mode = Mode::Dev;
    while let Some(flag) = args.next() {
        if flag == "--beta" {
            ensure!(mode == Mode::Dev, "--beta given twice");
            mode = Mode::Beta;
            continue;
        }
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
        mode,
        identity: identity.to_ascii_uppercase(),
        shell_profile: shell.context("mac-bundle needs --shell-profile")?.into(),
        brain_profile: brain.context("mac-bundle needs --brain-profile")?.into(),
        out: out.context("mac-bundle needs --out")?.into(),
    })
}

fn decode(bytes: Vec<u8>) -> Result<Profile> {
    let plist = pipe("/usr/bin/security", &["cms", "-D"], &bytes)?;
    read_profile(bytes, &plist)
}

/// The fields of a decoded profile's property list; `bytes` are the signed ones it came from.
fn read_profile(bytes: Vec<u8>, plist: &[u8]) -> Result<Profile> {
    let field = |key: &str, format: &str, expect: Option<&str>| {
        let mut args = vec!["-extract", key, format];
        if let Some(kind) = expect {
            args.extend(["-expect", kind]);
        }
        args.extend(["-o", "-", "-"]);
        pipe("/usr/bin/plutil", &args, plist).with_context(|| format!("the profile's {key}"))
    };
    let teams = strings("TeamIdentifier", &field("TeamIdentifier", "json", None)?)?;
    let [team] = <[String; 1]>::try_from(teams)
        .ok()
        .context("the profile names one team")?;
    let prefixes = strings(
        "ApplicationIdentifierPrefix",
        &field("ApplicationIdentifierPrefix", "json", None)?,
    )?;
    let [prefix] = <[String; 1]>::try_from(prefixes)
        .ok()
        .context("the profile names one App ID prefix")?;
    // One successful parse decides which optional keys are present: plutil writes each top-level
    // key on its own line, indented by one tab. A present key that then can't be read as its
    // type refuses the profile, so a failed read never passes for an absent key.
    let canonical = pipe(
        "/usr/bin/plutil",
        &["-convert", "xml1", "-o", "-", "-"],
        plist,
    )
    .context("the profile's property list")?;
    let canonical = String::from_utf8_lossy(&canonical);
    let present = |key: &str| {
        canonical.lines().any(|line| {
            line.strip_prefix("\t<key>")
                .and_then(|rest| rest.strip_suffix("</key>"))
                == Some(key)
        })
    };
    let all_devices = present("ProvisionsAllDevices")
        && field("ProvisionsAllDevices", "raw", Some("bool"))
            .ok()
            .context("the profile's ProvisionsAllDevices has the wrong type")?
            .trim_ascii()
            == b"true";
    let devices = if present("ProvisionedDevices") {
        let json = field("ProvisionedDevices", "json", None)
            .ok()
            .context("the profile's ProvisionedDevices has the wrong type")?;
        Some(strings("ProvisionedDevices", &json)?.len())
    } else {
        None
    };
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
        prefix,
        all_devices,
        devices,
        entitlements: object("Entitlements", &field("Entitlements", "json", None)?)?,
        expires: String::from_utf8(field("ExpirationDate", "raw", Some("date"))?)?
            .trim()
            .to_owned(),
        platforms: strings("Platform", &field("Platform", "json", None)?)?,
        certificates,
    })
}

/// A profile field's JSON, which must be an object. A conversion error can quote the value, so
/// only the field is named.
fn object(key: &str, json: &[u8]) -> Result<Value> {
    serde_json::from_slice::<Value>(json)
        .ok()
        .filter(Value::is_object)
        .with_context(|| format!("the profile's {key} has the wrong type"))
}

/// A profile field's JSON, which must be an array of strings, named the same way.
fn strings(key: &str, json: &[u8]) -> Result<Vec<String>> {
    let value: Option<Value> = serde_json::from_slice(json).ok();
    value
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .with_context(|| format!("the profile's {key} has the wrong type"))
}

/// The profile is a current macOS one from a well-formed team, for exactly `id`, authorizing
/// `identity`'s certificate, allowing both of the role's keychain groups, and of the kind the mode
/// signs with: Developer ID for beta, development otherwise.
fn check(
    profile: &Profile,
    id: &str,
    group: &str,
    identity: &str,
    now: &str,
    mode: Mode,
) -> Result<()> {
    let team = &profile.team;
    let prefix = &profile.prefix;
    let text = |key| profile.entitlements.get(key).and_then(Value::as_str);
    ensure!(identifier(team), "its team isn't a team identifier");
    ensure!(identifier(prefix), "its App ID prefix isn't one");
    ensure!(
        profile.platforms.iter().any(|platform| platform == "OSX"),
        "it isn't a macOS profile"
    );
    match mode {
        Mode::Beta => ensure!(
            profile.all_devices && profile.devices.is_none(),
            "it isn't a Developer ID profile"
        ),
        Mode::Dev => {
            ensure!(
                !profile.all_devices,
                "it's a Developer ID profile, which needs --beta"
            );
            ensure!(
                profile.devices.is_some_and(|count| count > 0),
                "it isn't a development profile: it lists no devices"
            );
        }
    }
    ensure!(utc_seconds(&profile.expires), "its expiry isn't a UTC time");
    // Both are ISO 8601 UTC with seconds, so text order is time order.
    ensure!(profile.expires.as_str() > now, "it has expired");
    ensure!(
        text(TEAM_ID) == Some(team.as_str()),
        "its team doesn't match its entitlements"
    );
    ensure!(
        text(APP_ID) == Some(format!("{prefix}.{id}").as_str()),
        "it isn't for exactly {id}"
    );
    ensure!(
        profile.certificates.iter().any(|hash| hash == identity),
        "it doesn't authorize the signing certificate"
    );
    for wanted in [id, group] {
        ensure!(
            allows(profile, &format!("{prefix}.{wanted}")),
            "it doesn't allow the keychain group {wanted}"
        );
    }
    Ok(())
}

/// Ten uppercase letters or digits, the form of a team and of an App ID prefix.
fn identifier(text: &str) -> bool {
    text.len() == 10
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Whether the profile grants keychain group `wanted`, exactly or by its prefix's wildcard.
fn allows(profile: &Profile, wanted: &str) -> bool {
    let any = format!("{}.*", profile.prefix);
    profile
        .entitlements
        .get(GROUPS)
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups
                .iter()
                .filter_map(Value::as_str)
                .any(|have| have == wanted || have == any)
        })
}

/// K-8 on a signed app as it will ship: read back its signature and its embedded profile, never
/// the ones this task asked for.
fn audit(app: &Path, id: &str, group: &str) -> Result<()> {
    let embedded = std::fs::read(app.join("Contents/embedded.provisionprofile"))
        .context("reading its embedded profile")?;
    let profile = decode(embedded).context("decoding its embedded profile")?;
    let (signed, display) = signature(app)?;
    audited(&profile, &signed, &display, id, group)
}

/// An app's signed entitlements, and `codesign -dvvv`'s description of its code directories.
fn signature(app: &Path) -> Result<(Value, Vec<u8>)> {
    let signed = command(
        "/usr/bin/codesign",
        &["-d", "--xml", "--entitlements", "-"],
        app,
    )?;
    // codesign writes the description to stderr, so the shell moves it to stdout.
    let mut display = Command::new("/bin/sh");
    display
        .args(["-c", "exec /usr/bin/codesign -dvvv \"$1\" 2>&1", "sh"])
        .arg(app);
    let display = bounded("codesign", &mut display, None, SIGN_WAIT)?;
    Ok((to_json(&signed)?, display))
}

/// The checks `audit` makes, on what it read: no debugger entitlement, the hardened runtime on
/// every code directory, and exactly the App ID, team and groups the embedded profile grants
/// under its own prefix.
fn audited(profile: &Profile, signed: &Value, display: &[u8], id: &str, group: &str) -> Result<()> {
    let prefix = &profile.prefix;
    ensure!(
        signed.get(GET_TASK_ALLOW).is_none(),
        "it's signed with get-task-allow"
    );
    ensure!(thin(display), "it isn't a single-architecture app");
    ensure!(
        one_digest(display),
        "it isn't signed with exactly one code directory"
    );
    ensure!(
        hardened(display),
        "it isn't signed with the hardened runtime"
    );
    let app_id = format!("{prefix}.{id}");
    let granted = |key| profile.entitlements.get(key).and_then(Value::as_str);
    ensure!(
        granted(APP_ID) == Some(app_id.as_str()),
        "its embedded profile isn't for {id}"
    );
    ensure!(
        signed.get(APP_ID).and_then(Value::as_str) == Some(app_id.as_str()),
        "its App ID isn't its profile's prefix and {id}"
    );
    ensure!(
        granted(TEAM_ID) == Some(profile.team.as_str())
            && signed.get(TEAM_ID).and_then(Value::as_str) == Some(profile.team.as_str()),
        "its team isn't its profile's"
    );
    let groups = [format!("{prefix}.{id}"), format!("{prefix}.{group}")];
    ensure!(
        signed.get(GROUPS) == Some(&json!(groups))
            && groups.iter().all(|wanted| allows(profile, wanted)),
        "its keychain groups aren't its profile's"
    );
    ensure!(
        *signed == entitlements(prefix, &profile.team, id, group),
        "it carries entitlements beyond its App ID, team and groups"
    );
    Ok(())
}

/// Whether `codesign -dv` describes a thin Mach-O: one architecture, so the one code directory
/// it shows is the whole signature. A universal app would show only the host's slice.
fn thin(display: &[u8]) -> bool {
    let text = String::from_utf8_lossy(display);
    let mut formats = text.lines().filter_map(|line| line.strip_prefix("Format="));
    formats
        .next()
        .is_some_and(|format| format.contains("Mach-O thin ("))
        && formats.next().is_none()
}

/// Whether `codesign -dvvv` lists exactly one hash choice: a slice signed with several digests
/// has alternate code directories that `-dv` describes as one.
fn one_digest(display: &[u8]) -> bool {
    let text = String::from_utf8_lossy(display);
    let mut choices = text
        .lines()
        .filter_map(|line| line.strip_prefix("Hash choices="));
    choices
        .next()
        .is_some_and(|choice| !choice.is_empty() && !choice.contains(','))
        && choices.next().is_none()
}

/// Whether `codesign -dv` shows exactly one code directory, with the runtime flag.
fn hardened(display: &[u8]) -> bool {
    let text = String::from_utf8_lossy(display);
    let flags: Vec<Option<u32>> = text
        .lines()
        .filter(|line| line.starts_with("CodeDirectory "))
        .map(|line| {
            let hex = line.split_once(" flags=0x")?.1;
            let end = hex
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(hex.len());
            u32::from_str_radix(&hex[..end], 16).ok()
        })
        .collect();
    matches!(flags.as_slice(), [Some(flags)] if flags & RUNTIME != 0)
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

/// The entitlements a role is signed with: its own App ID under the profile's prefix, which is
/// also its private keychain group, its team, and the instance group it shares.
fn entitlements(prefix: &str, team: &str, id: &str, group: &str) -> Value {
    json!({
        APP_ID: format!("{prefix}.{id}"),
        TEAM_ID: team,
        GROUPS: [format!("{prefix}.{id}"), format!("{prefix}.{group}")],
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

/// The Dev shell and brain binaries Cargo builds now, by executable name.
fn build() -> Result<Vec<(String, PathBuf)>> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut metadata = Command::new(&cargo);
    metadata.args(["metadata", "--format-version", "1", "--no-deps", "--locked"]);
    let metadata: Value =
        serde_json::from_slice(&bounded("cargo metadata", &mut metadata, None, TOOL_WAIT)?)
            .context("Cargo's metadata")?;
    let output = Command::new(cargo)
        .args([
            "build",
            "--locked",
            "--message-format=json-render-diagnostics",
        ])
        .args(["-p", "kumochi-shell", "-p", "kumochi-brain"])
        .stderr(Stdio::null())
        .output()?;
    // Cargo's diagnostics name local paths, so they aren't shown here.
    ensure!(
        output.status.success(),
        "the Dev build failed; `cargo build -p kumochi-shell -p kumochi-brain` shows why"
    );
    artifacts(&output.stdout, &members(&metadata)?)
}

/// Each workspace package's opaque Cargo ID, by name.
fn members(metadata: &Value) -> Result<Vec<(String, String)>> {
    let workspace = metadata["workspace_members"]
        .as_array()
        .context("Cargo's workspace members")?;
    let packages = metadata["packages"]
        .as_array()
        .context("Cargo's packages")?;
    Ok(packages
        .iter()
        .filter(|package| workspace.contains(&package["id"]))
        .filter_map(|package| {
            Some((
                package["name"].as_str()?.to_owned(),
                package["id"].as_str()?.to_owned(),
            ))
        })
        .collect())
}

/// Exactly one `compiler-artifact` of each workspace package's binary target, matched by the
/// package's exact ID.
fn artifacts(stdout: &[u8], members: &[(String, String)]) -> Result<Vec<(String, PathBuf)>> {
    let mut built = Vec::new();
    for (package, exe) in [("kumochi-shell", SHELL_EXE), ("kumochi-brain", BRAIN_EXE)] {
        let id = members
            .iter()
            .find(|(name, _)| name == package)
            .map(|(_, id)| id.as_str())
            .with_context(|| format!("the workspace has no {package}"))?;
        let mut found = stdout
            .split(|&byte| byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter(|message| {
                let target = &message["target"];
                message["reason"] == "compiler-artifact"
                    && message["package_id"] == id
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
    // shortcut: no secure timestamp or notarization, so a downloaded beta is refused by
    // Gatekeeper on other Macs; add both before the beta leaves the owner's Macs.
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
        .process_group(0)
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

/// Stops the process group `child` leads, then reaps it. The group is signalled while its
/// unreaped leader keeps the id from being reused.
fn end(child: &mut Child) -> Result<ExitStatus> {
    let stopped = stop_group(child.id());
    let status = reap(child)?;
    stopped?;
    Ok(status)
}

/// Kills `child` alone, whose group was stopped already, and reaps it.
fn reap(child: &mut Child) -> Result<ExitStatus> {
    if let Err(error) = child.kill() {
        // A leader that's still running after a failed kill isn't waited on.
        return child
            .try_wait()?
            .ok_or(error)
            .context("stopping a child process");
    }
    child.wait().context("reaping a child process")
}

/// Kills every process in group `group`.
fn stop_group(group: u32) -> Result<()> {
    let status = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("stopping a process group")?;
    ensure!(
        status.success(),
        "stopping a process group failed ({status})"
    );
    Ok(())
}

/// Runs `program` with `args` and then `path`, for at most [`SIGN_WAIT`]; its stdout.
fn command(program: &str, args: &[&str], path: &Path) -> Result<Vec<u8>> {
    let mut command = Command::new(program);
    command.args(args).arg(path);
    bounded(program, &mut command, None, SIGN_WAIT)
}

/// Runs `program` with `args` on `input`, for at most [`TOOL_WAIT`]; its stdout.
fn pipe(program: &str, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut command = Command::new(program);
    command.args(args);
    bounded(program, &mut command, Some(input), TOOL_WAIT)
}

/// Runs `command` in its own process group within `wait`, feeding `input` and draining both
/// outputs as it runs, so a large one can't stall it. At the deadline the whole group is stopped
/// once, so a helper holding a pipe ends too; a pipe still open [`STOP_GRACE`] later fails the
/// task instead of holding it. The child is reaped on every path, and only after its pipes settle.
/// Its stderr isn't shown, because it can name the team or the identity.
fn bounded(
    program: &str,
    command: &mut Command,
    input: Option<&[u8]>,
    wait: Duration,
) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let deadline = Instant::now() + wait;
    let mut child = command
        .process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let (sent, received) = std::sync::mpsc::channel();
    // Detached, not scoped: a pipe a helper outside the group keeps open must not hold the task.
    let feed = sent.clone();
    let stdin = child.stdin.take();
    let input = input.map(<[u8]>::to_vec);
    std::thread::spawn(move || {
        let written = match (stdin, input) {
            (Some(mut stdin), Some(input)) => stdin.write_all(&input),
            _ => Ok(()),
        };
        feed.send((0, written.map(|()| Vec::new()))).ok();
    });
    let outputs: [Option<Box<dyn std::io::Read + Send>>; 2] = [
        child
            .stdout
            .take()
            .map(|out| -> Box<dyn std::io::Read + Send> { Box::new(out) }),
        child
            .stderr
            .take()
            .map(|err| -> Box<dyn std::io::Read + Send> { Box::new(err) }),
    ];
    for (stream, pipe) in (1..).zip(outputs) {
        let sent = sent.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = match pipe {
                Some(mut pipe) => pipe.read_to_end(&mut bytes).map(|_| bytes),
                None => Ok(bytes),
            };
            sent.send((stream, read)).ok();
        });
    }
    drop(sent);
    let mut streams = [None, None, None];
    // Set once, when the group is stopped: every pipe still open shares this one grace.
    let mut grace: Option<Instant> = None;
    let mut gathered = Ok(());
    for _ in 0..3 {
        let left = deadline.saturating_duration_since(Instant::now());
        let next = received.recv_timeout(left).or_else(|_| {
            let until = *grace.get_or_insert_with(|| {
                // Refused only when no member is left to stop; the grace bounds the wait either way.
                stop_group(child.id()).ok();
                Instant::now() + STOP_GRACE
            });
            received
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .with_context(|| format!("{program}'s output stayed open after it was stopped"))
        });
        match next {
            Ok((stream, result)) => streams[stream] = Some(result),
            Err(error) => {
                gathered = Err(error);
                break;
            }
        }
    }
    let late = grace.is_some();
    let status = if late {
        reap(&mut child).map(Some)
    } else {
        wait_for(
            &mut child,
            deadline.saturating_duration_since(Instant::now()),
        )
    };
    gathered?;
    let status = status?
        .filter(|_| !late)
        .with_context(|| format!("{program} didn't finish within {wait:?}"))?;
    let [written, out, err] = streams.map(|stream| stream.context("a pipe result"));
    let err = err??;
    ensure!(
        status.success(),
        "{program} failed ({status}, {} bytes of stderr withheld)",
        err.len()
    );
    written??;
    out?.map_err(Into::into)
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
