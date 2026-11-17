#![forbid(unsafe_code)]
//! The PostgreSQL records: accounts, sessions, documents, roles and access assignments, the
//! revision index, the content store's safety rows and the event log. PostgreSQL never holds
//! document content; Git and the content store do.

mod accounts;
mod adoption;
mod bin;
mod blobs;
mod brand;
mod db;
mod documents;
mod events;
mod explorer;
mod groups;
mod libraries;
mod links;
mod live;
mod navigation;
pub mod policies;
mod quotas;
mod requests;
mod revisions;
mod roles;
mod scans;
mod scopes;
mod settings;
mod sites;
mod sources;
mod templates;
#[cfg(feature = "test-support")]
pub mod test_support;
mod tree;
mod uploads;

pub use accounts::{
    AccountError, NewUser, SESSION_LIFETIME, Session, SessionToken, SignInError, SignInFailure,
    User,
};
pub use adoption::{Adoption, Unplaced};
pub use bin::{BIN_DAYS, BinCursor, BinEntry, BinStage, BinState};
pub use blobs::{BLOB_REFS_DIRTY, BLOB_REFS_UPGRADE, ClaimId, StaleClaim};
pub use brand::{
    BRAND_CENTRE_OWNER, BrandChange, BrandNeeds, LookChange, Pin, org_pins, site_pins,
};
pub use db::{Db, DbError};
pub use documents::{AccessError, Document, Editor, Grant, validate_document_name};
pub use events::{Event, NewEvent};
pub use explorer::{
    AssignmentView, Break, ExplorerFilter, PrincipalView, RestorePreview, UserRef, breaks_csv,
    is_explorer_cursor,
};
pub use groups::{Group, SiteGroup};
pub use libraries::{
    IMAGE_TYPES, LibraryChange, LibraryKind, LibrarySettings, MAX_LIBRARY_TYPES, TemplateFile,
    is_extension, validate_library_fields, validate_library_settings,
};
pub use links::{AnyoneGrant, Link, LinkError, LinkKind, LinkToken};
pub use live::{
    GoneReason, LIVE_KEEP_DAYS, LiveEntryRow, LiveLoaded, LiveLocal, LiveParticipantRow,
    LiveSessionRow, LiveSwept, NewLiveCancel, NewLiveChange,
};
pub use navigation::{NavViewer, ResolvedItem, ResolvedTarget, SitesMirror};
pub use policies::{
    BLOCKED_FILE_TYPES, FileType, OrgPolicy, PolicyError, check_name, check_size,
    clean_blocked_types,
};
pub use quotas::{QUOTA_WARNING_PERCENT, QuotaError, Reservation, SiteUsage};
pub use requests::{
    AccessRequest, MyRequest, REQUEST_MESSAGE_MAX, REQUESTS_PER_DAY, RequestDecision, RequestError,
};
pub use revisions::{RevisionRecord, RevisionRow};
pub use roles::{Access, Perms, Principal, RoleDef, RoleRef};
pub use scans::{
    DueScan, HeldDocument, ICAP_PORT, MAX_SCAN_DETAIL, Origin, QuarantineEntry, RELEASE_REASON,
    SCANNER_MAX_BYTES, SCANNER_SETTINGS, SCANNER_TIMEOUT_SECONDS, SCANNER_TOO_LARGE,
    SCANNER_UNAVAILABLE, SCANNER_URL, ScanInfo, ScanMark, ScanResult, ScanState, ScannerSettings,
    ScannerUrl, clean_detail, retry_after, valid_scanner_setting,
};
pub use scopes::{Crumb, Cursor, Listed, NodeAccess};
pub use settings::{ANYONE_LINK_MAX_DAYS, ANYONE_LINKS, SharingSettings};
pub use sites::{
    AdminSite, DEFAULT_LANGUAGE, DEFAULT_TIME_ZONE, GLOBAL_NAVIGATION, NavScope, RegionalDefaults,
    SiteChange, SiteKind, SiteSettings, SiteState, TemplateRow, validate_description,
    validate_language, validate_time_zone,
};
pub use sources::{
    DEFAULT_SOURCE_MAX_BYTES, DEFAULT_SOURCE_MAX_FILES, MAX_POLL_SECONDS, MAX_SOURCE_ERROR_CHARS,
    MIN_POLL_SECONDS, NewSource, SOURCE_MAX_BYTES, SOURCE_MAX_FILES, SealedRow, SecretId,
    SourceChange, SourceConfig, SourceItem, SourceItemLocation, SourceKind, SourceLock,
    SourceState, SourceStatus, SourceSyncTransition, StatusUpdate,
};
pub use templates::{
    MAX_PACKAGE_BYTES, MAX_TEMPLATE_DEPTH, MAX_TEMPLATE_FOLDERS, MAX_TEMPLATE_LIBRARIES,
    check_template_size, validate_template_body, validate_template_description,
    validate_template_name,
};
pub use tree::{
    Node, NodeChange, NodeKind, NodeRow, NodesApplied, is_format_char, validate_node_name,
};
pub use uploads::{
    NameConflict, NewUpload, UPLOADS_PER_PERSON, UploadError, UploadSession, UploadTarget,
};
