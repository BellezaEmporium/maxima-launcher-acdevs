pub mod dip;
pub mod pre_dip;

use dip::DiPManifest;
use pre_dip::PreDiPManifest;
use quick_xml::DeError;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ManifestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Xml(#[from] DeError),
    #[error(transparent)]
    Native(#[from] crate::util::native::NativeError),
    #[error(transparent)]
    Registry(#[from] crate::util::registry::RegistryError),

    #[error("failed to decode DiPManifest. Weird encoding?")]
    Decode,
    #[error(
        "Unsupported Manifest.\nDiP Attempt: `{dip_attempt:?}`\nPreDiP Attempt: `{pre_dip_attempt:?}`"
    )]
    Unsupported {
        dip_attempt: Box<ManifestError>,
        pre_dip_attempt: Box<ManifestError>,
    },
    #[error("could not find install path for `{0}`")]
    NoInstallPath(String),
}

pub const MANIFEST_RELATIVE_PATH: &str = "__Installer/installerdata.xml";

#[async_trait::async_trait]
pub trait GameManifest: Send + std::fmt::Debug {
    async fn run_touchup(&self, install_path: &Path) -> Result<(), ManifestError>;
    fn execute_path(&self, trial: bool) -> Option<String>;
    fn version(&self) -> Option<String>;
    fn needs_touchup_on_locate(&self) -> bool;
}
#[async_trait::async_trait]
impl GameManifest for DiPManifest {
    async fn run_touchup(&self, install_path: &Path) -> Result<(), ManifestError> {
        let install_path = install_path.to_path_buf();
        self.run_touchup(&install_path).await
    }

    fn execute_path(&self, trial: bool) -> Option<String> {
        self.execute_path(trial)
            .map(std::string::ToString::to_string)
    }

    fn version(&self) -> Option<String> {
        Some(self.version().to_string())
    }

    fn needs_touchup_on_locate(&self) -> bool {
        self.buildMetaData()
            .featureFlags()
            .attr_forceTouchupInstallerAfterUpdate
            .eq_ignore_ascii_case("true")
    }
}

#[async_trait::async_trait]
impl GameManifest for PreDiPManifest {
    async fn run_touchup(&self, install_path: &Path) -> Result<(), ManifestError> {
        let install_path = install_path.to_path_buf();
        self.run_touchup(&install_path).await
    }

    fn execute_path(&self, _: bool) -> Option<String> {
        None // pre-dip games don't have an exe field, usually they put the exe in the "exclude" section for some reason
    }

    fn version(&self) -> std::option::Option<std::string::String> {
        Some(self.version().to_string())
    }

    fn needs_touchup_on_locate(&self) -> bool {
        false // pre-DiP games never had this concept
    }
}

/// https://www.reddit.com/r/rust/comments/11co87m/comment/ja4sy88
fn bytes_to_string(bytes: Vec<u8>) -> Option<String> {
    if let Ok(v) = String::from_utf8(bytes.clone()) {
        return Some(v);
    }

    let u16_bytes: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|a| u16::from_ne_bytes([a[0], a[1]]))
        .collect();

    String::from_utf16(&u16_bytes).ok()
}

pub fn from_bytes<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ManifestError> {
    let string = bytes_to_string(bytes.to_vec()).ok_or(ManifestError::Decode)?;
    Ok(quick_xml::de::from_str(&string)?)
}

pub async fn read(path: PathBuf) -> Result<Box<dyn GameManifest>, ManifestError> {
    let bytes = tokio::fs::read(&path).await?;

    if let Ok(m) = from_bytes::<DiPManifest>(&bytes) {
        return Ok(Box::new(m));
    }

    match from_bytes::<PreDiPManifest>(&bytes) {
        Ok(m) => Ok(Box::new(m)),
        Err(pre_dip_err) => Err(ManifestError::Unsupported {
            dip_attempt: Box::new(ManifestError::Decode),
            pre_dip_attempt: Box::new(pre_dip_err),
        }),
    }
}
