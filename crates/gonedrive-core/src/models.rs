//! Wire types for the subset of Microsoft Graph that GOnedrive uses.
//!
//! These mirror the JSON exactly; anything derived (local paths, sync state)
//! lives in `db` instead. Every field the service may omit is `Option`, because
//! Graph freely omits facets that do not apply — a folder has no `file` facet,
//! a deleted item has almost nothing at all.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A drive (a user's OneDrive, or a document library).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    #[serde(default)]
    pub drive_type: Option<String>,
    #[serde(default)]
    pub owner: Option<IdentitySet>,
    #[serde(default)]
    pub quota: Option<Quota>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IdentitySet {
    #[serde(default)]
    pub user: Option<Identity>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Present on work/school accounts; absent on personal ones.
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub used: Option<u64>,
    #[serde(default)]
    pub remaining: Option<u64>,
}

/// The signed-in user, from `/me`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub user_principal_name: Option<String>,
    /// Personal accounts report the address here instead.
    #[serde(default)]
    pub mail: Option<String>,
}

impl User {
    /// Best available human-readable identifier for the account.
    pub fn address(&self) -> Option<&str> {
        self.user_principal_name
            .as_deref()
            .or(self.mail.as_deref())
    }
}

/// A file or folder.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    /// Absent only on `deleted` tombstones in a delta feed.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "eTag")]
    pub etag: Option<String>,
    /// Changes only when *content* changes, unlike `eTag` which also tracks
    /// metadata. This is what the sync engine compares to decide on a download.
    #[serde(default, rename = "cTag")]
    pub ctag: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_modified_date_time: Option<OffsetDateTime>,
    #[serde(default)]
    pub parent_reference: Option<ParentReference>,
    #[serde(default)]
    pub file: Option<FileFacet>,
    #[serde(default)]
    pub folder: Option<FolderFacet>,
    #[serde(default)]
    pub package: Option<PackageFacet>,
    #[serde(default)]
    pub deleted: Option<DeletedFacet>,
    #[serde(default)]
    pub file_system_info: Option<FileSystemInfo>,
    /// Present on the drive root and nothing else.
    #[serde(default)]
    pub root: Option<serde_json::Value>,
    /// A short-lived pre-authenticated URL, only present when requested.
    #[serde(default, rename = "@microsoft.graph.downloadUrl")]
    pub download_url: Option<String>,
}

impl DriveItem {
    pub fn is_folder(&self) -> bool {
        self.folder.is_some()
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }

    pub fn is_root(&self) -> bool {
        self.root.is_some()
    }

    /// A OneNote notebook or similar. These present as folders but cannot be
    /// synced as ordinary trees, so the engine skips them rather than producing
    /// a half-broken local copy.
    pub fn is_package(&self) -> bool {
        self.package.is_some()
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.parent_reference.as_ref()?.id.as_deref()
    }

    /// The mtime to stamp on the local file: the caller-supplied filesystem
    /// time if the uploader set one, else the service's modification time.
    pub fn effective_mtime(&self) -> Option<OffsetDateTime> {
        self.file_system_info
            .as_ref()
            .and_then(|fsi| fsi.last_modified_date_time)
            .or(self.last_modified_date_time)
    }

    pub fn hashes(&self) -> Option<&Hashes> {
        self.file.as_ref()?.hashes.as_ref()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentReference {
    #[serde(default)]
    pub drive_id: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    /// e.g. `/drive/root:/Documents`
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    /// OneDrive for Business and SharePoint.
    #[serde(default)]
    pub quick_xor_hash: Option<String>,
    /// Personal OneDrive.
    #[serde(default)]
    pub sha1_hash: Option<String>,
    #[serde(default)]
    pub sha256_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderFacet {
    #[serde(default)]
    pub child_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageFacet {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeletedFacet {
    #[serde(default)]
    pub state: Option<String>,
}

/// Client-authored timestamps. Setting these on upload is what makes a
/// round-trip preserve the local mtime instead of stamping "now".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemInfo {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub created_date_time: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub last_modified_date_time: Option<OffsetDateTime>,
}

/// One page of a `/delta` enumeration.
#[derive(Debug, Clone, Deserialize)]
pub struct DeltaPage {
    #[serde(default)]
    pub value: Vec<DriveItem>,
    /// More pages of this same enumeration.
    #[serde(default, rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    /// Present on the final page; store it verbatim for the next round.
    #[serde(default, rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

/// A resumable upload session.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSession {
    pub upload_url: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expiration_date_time: Option<OffsetDateTime>,
    #[serde(default)]
    pub next_expected_ranges: Vec<String>,
}

impl UploadSession {
    /// First byte the service still wants.
    ///
    /// `nextExpectedRanges` describes which bytes are *missing*, not how large
    /// the next chunk should be, so only its start offset is meaningful here.
    pub fn next_offset(&self) -> Option<u64> {
        let range = self.next_expected_ranges.first()?;
        range.split('-').next()?.trim().parse().ok()
    }
}

/// The error envelope Graph returns on failure.
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_file_item() {
        let json = r#"{
            "id": "01ABC", "name": "report.docx", "size": 12345,
            "eTag": "\"{GUID},2\"", "cTag": "\"c:{GUID},1\"",
            "lastModifiedDateTime": "2026-08-20T10:11:12Z",
            "parentReference": {"driveId": "d1", "id": "01ROOT", "path": "/drive/root:"},
            "file": {"mimeType": "application/vnd.openxmlformats", "hashes": {"quickXorHash": "AAAA"}},
            "fileSystemInfo": {"lastModifiedDateTime": "2026-08-19T08:00:00Z"}
        }"#;
        let item: DriveItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.name.as_deref(), Some("report.docx"));
        assert!(!item.is_folder() && !item.is_deleted());
        assert_eq!(item.parent_id(), Some("01ROOT"));
        assert_eq!(item.hashes().unwrap().quick_xor_hash.as_deref(), Some("AAAA"));
        // fileSystemInfo wins over the service timestamp.
        assert_eq!(
            item.effective_mtime().unwrap(),
            time::macros::datetime!(2026-08-19 08:00:00 UTC)
        );
    }

    #[test]
    fn parses_a_delete_tombstone_without_a_name() {
        let json = r#"{"id": "01GONE", "deleted": {"state": "deleted"},
                       "parentReference": {"driveId": "d1", "id": "01ROOT"}}"#;
        let item: DriveItem = serde_json::from_str(json).unwrap();
        assert!(item.is_deleted());
        assert!(item.name.is_none());
    }

    #[test]
    fn parses_a_delta_page_with_both_links() {
        let json = r#"{"value": [], "@odata.nextLink": "https://next"}"#;
        let page: DeltaPage = serde_json::from_str(json).unwrap();
        assert_eq!(page.next_link.as_deref(), Some("https://next"));
        assert!(page.delta_link.is_none());

        let json = r#"{"value": [], "@odata.deltaLink": "https://delta?token=x"}"#;
        let page: DeltaPage = serde_json::from_str(json).unwrap();
        assert_eq!(page.delta_link.as_deref(), Some("https://delta?token=x"));
    }

    #[test]
    fn upload_session_reports_the_missing_start_offset() {
        let session = UploadSession {
            upload_url: "https://u".into(),
            expiration_date_time: None,
            next_expected_ranges: vec!["26214400-".into()],
        };
        assert_eq!(session.next_offset(), Some(26_214_400));

        let empty = UploadSession {
            next_expected_ranges: vec![],
            ..session
        };
        assert_eq!(empty.next_offset(), None);
    }

    #[test]
    fn packages_are_distinguished_from_folders() {
        let json = r#"{"id":"01NB","name":"Notebook","folder":{"childCount":3},
                       "package":{"type":"oneNote"}}"#;
        let item: DriveItem = serde_json::from_str(json).unwrap();
        assert!(item.is_folder());
        assert!(item.is_package());
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        // Graph adds facets over time; an unknown one must not fail a sync.
        let json = r#"{"id":"1","name":"a","someNewFacet":{"x":1},"@odata.etag":"z"}"#;
        assert!(serde_json::from_str::<DriveItem>(json).is_ok());
    }
}
