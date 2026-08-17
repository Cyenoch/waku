use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ts_rs::TS;

pub const ATTACHMENT_SCHEME: &str = "waku-attachment:";
pub const MAX_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_ATTACHMENT_FILES: usize = 4_096;

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AttachmentUpload {
    File { data_base64: String },
    Directory { entries: Vec<AttachmentUploadEntry> },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentUploadEntry {
    #[ts(type = "string")]
    pub relative_path: PathBuf,
    pub data_base64: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StoredAttachment {
    pub reference: String,
    #[ts(type = "string")]
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// A user prompt plus daemon-owned image references.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PromptInput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<PromptImageRef>,
}

impl PromptInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
        }
    }
}

/// Image payload already stored by the daemon. Never a filesystem path or
/// inline base64 — the daemon resolves the typed reference once.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PromptImageRef {
    Blob { reference: String },
    Attachment { reference: String },
}

impl PromptImageRef {
    pub fn reference(&self) -> &str {
        match self {
            Self::Blob { reference } | Self::Attachment { reference } => reference,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Blob { reference } if reference.starts_with(crate::blob::SCHEME) => Ok(()),
            Self::Attachment { reference } if reference.starts_with(ATTACHMENT_SCHEME) => Ok(()),
            Self::Blob { reference } => Err(format!("not a blob reference: {reference}")),
            Self::Attachment { reference } => {
                Err(format!("not an attachment reference: {reference}"))
            }
        }
    }
}
