use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ts_rs::TS;

pub const ATTACHMENT_SCHEME: &str = "wakuwaku-attachment:";
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

/// A user prompt plus daemon-owned image references and safe source metadata.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PromptInput {
    pub text: String,
    /// User-visible text when it differs from the provider-facing `text`
    /// (attachment mentions appended, slash-command expansion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<PromptImageRef>,
    /// Mention/name/type metadata for trajectory recording. Never a host path
    /// or image base64 — those stay on the daemon after resolution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<PromptAttachmentSource>,
}

impl PromptInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            display_text: None,
            attachments: Vec::new(),
            sources: Vec::new(),
        }
    }
}

/// Safe attachment metadata carried with a prompt. `reference` is a daemon
/// blob/attachment scheme only — never a filesystem path or inline bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PromptAttachmentSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub mention: String,
    pub name: String,
    pub is_dir: bool,
    pub is_image: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

impl PromptAttachmentSource {
    pub fn from_named_attachment(
        reference: Option<String>,
        mention: impl Into<String>,
        name: impl Into<String>,
        is_dir: bool,
        is_image: bool,
    ) -> Self {
        let name = name.into();
        let mime = if is_dir {
            None
        } else {
            Self::mime_from_name(&name)
        };
        Self {
            reference: Self::sanitize_reference(reference),
            mention: mention.into(),
            name,
            is_dir,
            is_image,
            mime,
        }
    }

    pub fn sanitize_reference(reference: Option<String>) -> Option<String> {
        reference.filter(|value| is_stored_reference(value))
    }

    pub fn mime_from_name(name: &str) -> Option<String> {
        let ext = std::path::Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())?;
        Some(
            match ext.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "svg" => "image/svg+xml",
                "bmp" => "image/bmp",
                "tif" | "tiff" => "image/tiff",
                "avif" => "image/avif",
                "heic" => "image/heic",
                _ => return None,
            }
            .to_owned(),
        )
    }

    /// Blob/attachment scheme only. Host paths and data URLs are dropped.
    pub fn safe_reference(&self) -> Option<&str> {
        self.reference
            .as_deref()
            .filter(|reference| is_stored_reference(reference))
    }
}

fn is_stored_reference(value: &str) -> bool {
    crate::blob::is_reference(value) || value.starts_with(ATTACHMENT_SCHEME)
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

    pub fn from_stored_reference(reference: impl Into<String>) -> Option<Self> {
        let reference = reference.into();
        if crate::blob::is_reference(&reference) {
            Some(Self::Blob { reference })
        } else if reference.starts_with(ATTACHMENT_SCHEME) {
            Some(Self::Attachment { reference })
        } else {
            None
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_input_text_helper_leaves_metadata_empty() {
        let input = PromptInput::text("hello");
        assert_eq!(input.text, "hello");
        assert_eq!(input.display_text, None);
        assert!(input.attachments.is_empty());
        assert!(input.sources.is_empty());
    }

    #[test]
    fn prompt_input_round_trips_display_text_and_sources() {
        let input = PromptInput {
            text: "see @notes.md".into(),
            display_text: Some("see".into()),
            attachments: vec![PromptImageRef::Blob {
                reference: "wakuwaku-blob:pic.png".into(),
            }],
            sources: vec![PromptAttachmentSource::from_named_attachment(
                Some("wakuwaku-blob:pic.png".into()),
                "pic.png",
                "pic.png",
                false,
                true,
            )],
        };
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json["text"], "see @notes.md");
        assert_eq!(json["displayText"], "see");
        assert_eq!(json["attachments"][0]["kind"], "blob");
        assert_eq!(json["sources"][0]["mention"], "pic.png");
        assert_eq!(json["sources"][0]["name"], "pic.png");
        assert_eq!(json["sources"][0]["isDir"], false);
        assert_eq!(json["sources"][0]["isImage"], true);
        assert_eq!(json["sources"][0]["mime"], "image/png");
        assert_eq!(json["sources"][0]["reference"], "wakuwaku-blob:pic.png");
        assert_eq!(serde_json::from_value::<PromptInput>(json).unwrap(), input);
    }

    #[test]
    fn prompt_input_omits_empty_optional_fields() {
        let json = serde_json::to_value(PromptInput::text("plain")).unwrap();
        assert_eq!(json, json!({ "text": "plain" }));
    }

    #[test]
    fn prompt_input_deserializes_text_only_payloads() {
        let input: PromptInput = serde_json::from_value(json!({ "text": "hi" })).unwrap();
        assert_eq!(input, PromptInput::text("hi"));
    }

    #[test]
    fn stored_reference_accepts_blob_and_attachment_schemes_only() {
        assert!(matches!(
            PromptImageRef::from_stored_reference("wakuwaku-blob:a.png"),
            Some(PromptImageRef::Blob { .. })
        ));
        assert!(matches!(
            PromptImageRef::from_stored_reference("wakuwaku-attachment:dir"),
            Some(PromptImageRef::Attachment { .. })
        ));
        assert_eq!(
            PromptImageRef::from_stored_reference("/tmp/secret.png"),
            None
        );
        assert_eq!(
            PromptImageRef::from_stored_reference("data:image/png;base64,aaaa"),
            None
        );
    }

    #[test]
    fn attachment_source_safe_reference_drops_paths_and_base64() {
        let source = PromptAttachmentSource::from_named_attachment(
            Some("/var/waku/attachments/x/photo.png".into()),
            "photo.png",
            "photo.png",
            false,
            true,
        );
        assert_eq!(source.reference, None);
        assert_eq!(source.safe_reference(), None);
        assert_eq!(source.mime.as_deref(), Some("image/png"));

        let data_url = PromptAttachmentSource {
            reference: Some("data:image/png;base64,aaaa".into()),
            mention: "photo".into(),
            name: "photo.png".into(),
            is_dir: false,
            is_image: true,
            mime: Some("image/png".into()),
        };
        assert_eq!(data_url.safe_reference(), None);

        let blob = PromptAttachmentSource::from_named_attachment(
            Some("wakuwaku-blob:photo.png".into()),
            "photo.png",
            "photo.png",
            false,
            true,
        );
        assert_eq!(blob.safe_reference(), Some("wakuwaku-blob:photo.png"));
    }
}
