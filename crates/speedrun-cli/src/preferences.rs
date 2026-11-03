//! CLI values for the device-wide capture preference.
use clap::ValueEnum;
pub use speedrun_core::preferences::CaptureMode;

use speedrun_core::preferences::Preferences;
use std::fs::{self, OpenOptions};
use std::io::{Error, ErrorKind, Result, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn read() -> Result<CaptureMode> {
    Ok(read_all()?.capture)
}

pub fn read_all() -> Result<Preferences> {
    read_at(&speedrun_core::paths::data_dir().join("preferences.json"))
}

fn read_at(path: &Path) -> Result<Preferences> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice::<Preferences>(&bytes).map_err(|e| Error::new(ErrorKind::InvalidData, e))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Preferences::default()),
        Err(e) => Err(e),
    }
}

pub fn set(mode: CaptureMode) -> Result<()> {
    update_at(
        &speedrun_core::paths::data_dir().join("preferences.json"),
        |prefs| prefs.capture = mode,
    )
}

pub fn set_retention(days: Option<u32>) -> Result<()> {
    if days == Some(0) {
        return Err(Error::new(ErrorKind::InvalidInput, "retention must be positive"));
    }
    update_at(
        &speedrun_core::paths::data_dir().join("preferences.json"),
        |prefs| prefs.retention_days = days,
    )
}

fn update_at(path: &Path, update: impl FnOnce(&mut Preferences)) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "preferences path has no parent"))?;
    fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("preferences.lock"))?;
    lock.lock()?;
    let mut prefs = read_at(path)?;
    update(&mut prefs);
    let temp = dir.join(format!(".preferences-{}.tmp", speedrun_core::new_id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        serde_json::to_writer(&mut file, &prefs)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub fn parse_retention(value: &str) -> std::result::Result<Option<u32>, String> {
    if value == "forever" {
        return Ok(None);
    }
    match value.parse::<u32>() {
        Ok(days) if days > 0 => Ok(Some(days)),
        _ => Err("use a positive number of days or 'forever'".into()),
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CaptureArgument {
    Content,
    MetadataOnly,
}
impl From<CaptureArgument> for CaptureMode {
    fn from(value: CaptureArgument) -> Self {
        match value {
            CaptureArgument::Content => Self::Content,
            CaptureArgument::MetadataOnly => Self::MetadataOnly,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn preference_defaults_persists_privately_and_invalid_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        assert_eq!(read_at(&path).unwrap().capture, CaptureMode::Content);
        std::fs::write(&path, r#"{"capture":"metadata_only"}"#).unwrap();
        assert_eq!(read_at(&path).unwrap().retention_days, None);
        update_at(&path, |prefs| prefs.capture = CaptureMode::MetadataOnly).unwrap();
        update_at(&path, |prefs| prefs.retention_days = Some(30)).unwrap();
        let prefs = read_at(&path).unwrap();
        assert_eq!(prefs.capture, CaptureMode::MetadataOnly);
        assert_eq!(prefs.retention_days, Some(30));
        update_at(&path, |prefs| prefs.capture = CaptureMode::Content).unwrap();
        assert_eq!(read_at(&path).unwrap().retention_days, Some(30));
        assert!(parse_retention("0").is_err());
        assert_eq!(parse_retention("forever").unwrap(), None);
        assert_eq!(parse_retention("12").unwrap(), Some(12));
        std::fs::write(&path, r#"{"capture":"content","retention_days":0}"#).unwrap();
        assert!(read_at(&path).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&path, "invalid").unwrap();
        assert!(read_at(&path).is_err());
    }
}
