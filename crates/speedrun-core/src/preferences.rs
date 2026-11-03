//! Shared format for device-wide prompt capture preferences.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    #[default]
    Content,
    MetadataOnly,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub capture: CaptureMode,
    /// None retains text until manually erased; a value must be positive.
    #[serde(default, deserialize_with = "retention_days")]
    pub retention_days: Option<u32>,
}

fn retention_days<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let days = Option::<u32>::deserialize(deserializer)?;
    if days == Some(0) {
        return Err(serde::de::Error::custom("retention_days must be positive"));
    }
    Ok(days)
}
