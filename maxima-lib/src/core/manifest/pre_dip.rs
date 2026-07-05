#![allow(non_snake_case)]

use crate::{
    core::manifest::{ManifestError, bytes_to_string},
    util::native::platform_path,
};
use derive_getters::Getters;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Default, Debug, Clone, Deserialize, PartialEq)]
pub struct PreDiPContentIDs {
    #[serde(rename = "contentID", default)]
    pub items: Vec<String>,
}

#[derive(Default, Debug, Clone, Deserialize, PartialEq)]
pub struct PreDiPUninstall {
    pub path: String,
}

#[derive(Default, Debug, Clone, Deserialize, PartialEq)]
pub struct PreDiPInstallManifest {
    #[serde(rename = "filePath")]
    pub file_path: String,
}

macro_rules! predip_type {
    (
        $(#[$message_attr:meta])*
        $message_name:ident;
        attr {
            $(
                $(#[$attr_field_attr:meta])*
                $attr_field:ident: $attr_field_type:ty
            ),* $(,)?
        },
        data {
            $(
                $(#[$field_attr:meta])*
                $field:ident: $field_type:ty
            ),* $(,)?
        }
    ) => {
        pastey::paste! {
            $(#[$message_attr])*
            #[derive(Default, Debug, Clone, Deserialize, PartialEq, Getters)]
            pub struct [<PreDiP $message_name>] {
                $(
                    $(#[$attr_field_attr])*
                    #[serde(rename = "@" $attr_field)]
                    pub [<attr_ $attr_field>]: $attr_field_type,
                )*
                $(
                    $(#[$field_attr])*
                    pub $field: $field_type,
                )*
            }
        }
    }
}

predip_type!(
    FeatureFlags;
    attr {
        forceTouchupInstallerAfterUpdate: String,
        autoUpdateEnabled: String,
        treatUpdatesAsMandatoryEnabled: String,
        useGameVersionFromManifestEnabled: String,
        UnableDifferentialUpdate: String,
    },
    data {}
);

predip_type!(
    Os;
    attr {
        minVersion: String,
    },
    data {}
);

predip_type!(
    Executable;
    attr {},
    data {
        filePath: String,
        parameters: String,
        updateParameters: String,
        repairParameters: String,
    }
);

predip_type!(
    LocaleInfo;
    attr {
        locale: String,
    },
    data {
        title: String,
        eula: String,
        #[serde(default)]
        exclude: Vec<String>,
    }
);

predip_type!(
    Metadata;
    attr {},
    data {
        featureFlags: PreDiPFeatureFlags,
        os: PreDiPOs,
        #[serde(default)]
        ignore: Vec<String>,
        #[serde(default)]
        localeInfo: Vec<PreDiPLocaleInfo>,
    }
);

predip_type!(
    Manifest;
    attr {
        gameVersion: String,
        manifestVersion: String,
    },
    data {
        #[serde(rename = "contentIDs")]
        contentIds: PreDiPContentIDs,
        uninstall: PreDiPUninstall,
        metadata: PreDiPMetadata,
        executable: PreDiPExecutable,
        installManifest: PreDiPInstallManifest,
    }
);

fn remove_leading_slash(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

fn remove_trailing_slash(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

fn remove_trailing_backslash(path: &str) -> &str {
    path.strip_suffix('\\').unwrap_or(path)
}

impl PreDiPManifest {
    pub async fn read(path: &Path) -> Result<Self, ManifestError> {
        let bytes = tokio::fs::read(path).await?;
        let string = bytes_to_string(bytes).ok_or(ManifestError::Decode)?;
        Ok(quick_xml::de::from_str(&string)?)
    }

    pub fn version(&self) -> &str {
        &self.attr_gameVersion
    }

    pub fn locale(&self) -> &str {
        self.metadata
            .localeInfo
            .iter()
            .find(|l| l.attr_locale == "en_US")
            .map(|l| l.attr_locale.as_str())
            .or_else(|| {
                self.metadata
                    .localeInfo
                    .first()
                    .map(|l| l.attr_locale.as_str())
            })
            .unwrap_or("en_US")
    }

    fn collect_touchup_args(
        &self,
        install_path: &Path,
        locale: &str,
    ) -> Result<Vec<String>, ManifestError> {
        let install_str = platform_path(
            remove_trailing_backslash(install_path.to_str().ok_or(ManifestError::Decode)?)
                .replace('/', "\\"),
        )
        .to_str()
        .ok_or(ManifestError::Decode)?
        .to_string();

        let expanded = self
            .executable
            .parameters
            .replace("{locale}", locale)
            .replace("{installLocation}", &install_str);

        Self::split_args(&expanded)
    }

    fn split_args(s: &str) -> Result<Vec<String>, ManifestError> {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;

        for ch in s.chars() {
            match ch {
                '"' => in_quotes = !in_quotes,
                ' ' if !in_quotes => {
                    if !current.is_empty() {
                        args.push(current.clone());
                        current.clear();
                    }
                }
                _ => current.push(ch),
            }
        }

        if in_quotes {
            // Unterminated quote — malformed manifest
            return Err(ManifestError::Decode);
        }

        if !current.is_empty() {
            args.push(current);
        }

        Ok(args)
    }

    #[cfg(unix)]
    pub async fn run_touchup(&self, install_path: &Path) -> Result<(), ManifestError> {
        use crate::{
            core::launch::mx_linux_setup,
            unix::{
                fs::case_insensitive_path,
                wine::{CommandType, invalidate_mx_wine_registry, run_wine_command},
            },
        };

        mx_linux_setup().await?;

        let install_path = PathBuf::from(remove_trailing_slash(
            install_path.to_str().ok_or(ManifestError::Decode)?,
        ));
        let args = self.collect_touchup_args(&install_path, self.locale())?;
        let path = case_insensitive_path(
            install_path.join(remove_leading_slash(&self.executable.filePath)),
        );

        run_wine_command(path, Some(args), None, true, CommandType::Run).await?;
        invalidate_mx_wine_registry().await;
        Ok(())
    }

    #[cfg(windows)]
    pub async fn run_touchup(&self, install_path: &Path) -> Result<(), ManifestError> {
        use crate::util::native::NativeError;
        use tokio::process::Command;

        let args = self.collect_touchup_args(install_path, self.locale())?;
        let path = install_path.join(remove_leading_slash(&self.executable.filePath));

        let status = Command::new(&path).args(&args).spawn()?.wait().await?;

        if !status.success() {
            return Err(ManifestError::Native(NativeError::Command(
                status.code().unwrap_or(0),
            )));
        }

        Ok(())
    }
}
