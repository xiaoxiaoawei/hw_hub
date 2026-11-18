//! Supported device families and their ESP image/bootloader metadata.

use anyhow::Result;
use espflash::target::Chip;
use serde::Serialize;
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChipModel {
    Esp32,
    Esp32s2,
    Esp32s3,
    Esp32c2,
    #[default]
    Esp32c3,
    Esp32c6,
    Esp32h2,
}

impl ChipModel {
    pub fn all() -> &'static [Self] {
        &[
            Self::Esp32,
            Self::Esp32s2,
            Self::Esp32s3,
            Self::Esp32c2,
            Self::Esp32c3,
            Self::Esp32c6,
            Self::Esp32h2,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Esp32 => "ESP32",
            Self::Esp32s2 => "ESP32-S2",
            Self::Esp32s3 => "ESP32-S3",
            Self::Esp32c2 => "ESP32-C2",
            Self::Esp32c3 => "ESP32-C3",
            Self::Esp32c6 => "ESP32-C6",
            Self::Esp32h2 => "ESP32-H2",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Esp32 => "esp32",
            Self::Esp32s2 => "esp32s2",
            Self::Esp32s3 => "esp32s3",
            Self::Esp32c2 => "esp32c2",
            Self::Esp32c3 => "esp32c3",
            Self::Esp32c6 => "esp32c6",
            Self::Esp32h2 => "esp32h2",
        }
    }

    pub fn esp_chip(self) -> Chip {
        match self {
            Self::Esp32 => Chip::Esp32,
            Self::Esp32s2 => Chip::Esp32s2,
            Self::Esp32s3 => Chip::Esp32s3,
            Self::Esp32c2 => Chip::Esp32c2,
            Self::Esp32c3 => Chip::Esp32c3,
            Self::Esp32c6 => Chip::Esp32c6,
            Self::Esp32h2 => Chip::Esp32h2,
        }
    }

    pub fn image_chip_id(self) -> u16 {
        self.esp_chip().id()
    }

    pub fn bootloader_address(self) -> u32 {
        self.esp_chip().boot_address()
    }
}

impl fmt::Display for ChipModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

impl FromStr for ChipModel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let normalized = value.to_ascii_lowercase().replace(['-', '_'], "");
        Self::all()
            .iter()
            .copied()
            .find(|chip| chip.slug() == normalized)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "不支持的芯片 {value}，可选：{}",
                    Self::all()
                        .iter()
                        .map(|chip| chip.slug())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_ids_and_bootloader_offsets_match_supported_esp_images() {
        for (chip, id, offset) in [
            (ChipModel::Esp32, 0, 0x1000),
            (ChipModel::Esp32s2, 2, 0x1000),
            (ChipModel::Esp32s3, 9, 0),
            (ChipModel::Esp32c2, 12, 0),
            (ChipModel::Esp32c3, 5, 0),
            (ChipModel::Esp32c6, 13, 0),
            (ChipModel::Esp32h2, 16, 0),
        ] {
            assert_eq!(chip.image_chip_id(), id);
            assert_eq!(chip.bootloader_address(), offset);
        }
        assert_eq!(ChipModel::default(), ChipModel::Esp32c3);
    }

    #[test]
    fn accepts_explicit_supported_names_and_rejects_unsupported_devices() {
        for chip in ChipModel::all() {
            assert_eq!(chip.slug().parse::<ChipModel>().unwrap(), *chip);
            assert_eq!(chip.label().parse::<ChipModel>().unwrap(), *chip);
        }
        assert!("esp8266".parse::<ChipModel>().is_err());
        assert!("esp32p4".parse::<ChipModel>().is_err());
    }
}
