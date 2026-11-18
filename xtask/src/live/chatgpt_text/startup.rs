use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf, Prefix};

use anyhow::{Result, anyhow, ensure};
use kumochi_protocol::constants::{
    AUTHORITY_FILE, CLAUDE_CODE_DELETING, CONFIG_FILE, DATA_DIR, STORE_FILE,
};
use rusqlite::Connection;

use crate::slice2::store;

pub(super) fn plan(budget: u32) -> Result<String> {
    let root = local_path("KUMOCHI_LIVE_TEXT_PROFILE")?;
    let local = local_path("LOCALAPPDATA")?;
    let roaming = local_path("APPDATA")?;
    let everyday = crate::platform::local_app_data()
        .and_then(|path| Ok(path.canonicalize()?))
        .map_err(|_| anyhow!("cannot identify ordinary local app data"))?;
    ensure!(
        local != everyday && local == root.join("local") && roaming == root.join("roaming"),
        "the folders do not match the disposable profile"
    );
    check_claude_cleanup(&everyday.join(DATA_DIR))?;
    let data = local.join(DATA_DIR);
    let db = data.join(STORE_FILE);
    let authority = data.join(AUTHORITY_FILE);
    let config = roaming.join(DATA_DIR).join(CONFIG_FILE);
    ensure!(
        [&db, &authority, &config]
            .into_iter()
            .all(|path| { path.canonicalize().is_ok_and(|resolved| resolved == *path) }),
        "profile files are missing or outside the disposable profile"
    );
    let db = store::open(&db).map_err(|_| anyhow!("cannot open the disposable store read-only"))?;
    let authority =
        store::open(&authority).map_err(|_| anyhow!("cannot open the authority read-only"))?;
    let config = File::open(config)
        .and_then(|file| io::read_to_string(file.take(131_073)))
        .map_err(|_| anyhow!("cannot read text-only settings"))?;
    check(&db, &authority, &config, budget)
}

pub(super) fn check_claude_cleanup(known_data: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(known_data.join(CLAUDE_CODE_DELETING))
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
        "Claude Code cleanup is pending or unknown"
    );
    Ok(())
}

fn local_path(name: &str) -> Result<PathBuf> {
    let path =
        PathBuf::from(std::env::var_os(name).ok_or_else(|| anyhow!("missing profile folder"))?);
    ensure!(path.is_absolute(), "profile folders must be absolute");
    let path = path
        .canonicalize()
        .map_err(|_| anyhow!("cannot resolve profile folder"))?;
    ensure!(
        matches!(path.components().next(), Some(std::path::Component::Prefix(part))
        if matches!(part.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
        "profile must use a local drive"
    );
    Ok(path)
}

pub(super) fn check(
    db: &Connection,
    authority: &Connection,
    config: &str,
    budget: u32,
) -> Result<String> {
    let empty = db.query_row(
        "SELECT (SELECT count(*) FROM threads) = 1 AND EXISTS(SELECT 1 FROM threads WHERE kind = 'main' AND state = 'live' AND model IS NULL)
          AND NOT EXISTS(SELECT 1 FROM runs) AND NOT EXISTS(SELECT 1 FROM events) AND NOT EXISTS(SELECT 1 FROM summaries)
          AND NOT EXISTS(SELECT 1 FROM actions) AND NOT EXISTS(SELECT 1 FROM purge_pending)",
        [], |row| row.get::<_, bool>(0),
    ).map_err(|_| anyhow!("cannot verify the disposable store"))?;
    ensure!(empty, "the profile has history or a pending store action");
    let clean = authority
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM generations WHERE state = 'active' AND kind = 'host')
          AND EXISTS(SELECT 1 FROM generations WHERE state = 'active' AND kind = 'chatgpt')
          AND NOT EXISTS(SELECT 1 FROM generations WHERE state = 'pending'
              OR (state = 'active' AND kind NOT IN ('host', 'chatgpt')))
          AND json_type(marker, '$.v') = 'integer' AND json_extract(marker, '$.v') = 1
          AND json_type(marker, '$.sign_out') = 'null'
          AND json_type(marker, '$.sweep') = 'false'
          AND json_type(marker, '$.request_pending') = 'false'
          AND json_type(marker, '$.outcome_unknown') = 'false'
          AND json_type(marker, '$.notice') = 'false'
          AND (SELECT count(*) FROM json_each(marker)) = 6
          FROM control WHERE id = 1 AND (SELECT user_version FROM pragma_user_version) = 3",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|_| anyhow!("cannot verify the credential authority metadata"))?;
    ensure!(
        clean,
        "connection kinds or pending authority work are not approved"
    );
    ensure!(text_only(config), "settings are not text-only");
    Ok(format!(
        "model_budget={budget}\nconnections=chatgpt\nchatgpt_startup_identity_validation=one possible check not visible in metadata (discovery and up to two signing-key requests)\n"
    ))
}

fn text_only(config: &str) -> bool {
    if config.len() > 131_072 {
        return false;
    }
    let Ok(config) = config.parse::<toml::Table>() else {
        return false;
    };
    let Some(voice) = config.get("voice").and_then(toml::Value::as_table) else {
        return false;
    };
    config
        .get("claude_code")
        .and_then(|value| value.get("enabled"))
        .and_then(toml::Value::as_bool)
        == Some(false)
        && ["hold_to_talk", "screen_holds"]
            .iter()
            .all(|key| voice.get(*key).and_then(toml::Value::as_bool) == Some(false))
        && ["show_screen", "talk_back"].iter().all(|key| {
            voice
                .get(*key)
                .is_none_or(|value| value.as_bool() == Some(false))
        })
        && ["installed", "removing"].iter().all(|key| {
            voice
                .get(*key)
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
        })
}
