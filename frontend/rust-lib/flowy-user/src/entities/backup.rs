use flowy_derive::ProtoBuf;

/// Input for the BackupWorkspace event.
///
/// Intentionally carries no filesystem path: the backend derives the snapshot
/// staging directory itself under the app data root. Accepting a caller-supplied
/// path here would let a compromised renderer write the full unencrypted
/// workspace database to an arbitrary location (path traversal / arbitrary write).
#[derive(ProtoBuf, Default)]
pub struct BackupWorkspacePB {}
