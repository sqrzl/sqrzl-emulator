//! Object publication has one durable decision: a synced `.publication.json`
//! after the private payload and metadata have been synced. Earlier failures
//! keep the public generation; later failures roll the decided generation
//! forward before reads or startup index reconstruction. Successful recovery
//! of an ambiguous commit is reported as success. If it cannot complete, the
//! API fails closed until the missing filesystem condition is repaired.
//!
//! Files and changed directory entries are synced before success. Cleanup
//! failures after retiring the decision may return an error with the complete
//! generation already present; callers must inspect state before retrying.
//! Versioned retries can create another version because this store does not
//! provide request idempotency. Multipart object commit precedes session
//! retirement, so interruption can leave a coherent object and a retryable
//! session; retirement uses a synced rename so partial cleanup is never listed.
//!
//! Qualification covers deterministic process termination, not power loss,
//! disk corruption, or filesystems that do not honor sync/rename semantics.
//! Callers must own the root writer guard before startup recovery.

use super::{FilesystemStorage, ObjectPayload};
use crate::error::{Error, Result};
use crate::models::Object;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use uuid::Uuid;

const JOURNAL: &str = ".publication.json";
const STAGE_PREFIX: &str = ".publication-stage-";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    stage: String,
    publish: bool,
    clear_current: bool,
    remove_versions: Vec<String>,
    marker: Option<Object>,
}

impl FilesystemStorage {
    pub(super) fn sync_directory(path: &Path) -> Result<()> {
        fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                Error::InternalError(format!(
                    "Failed to sync directory {}: {error}",
                    path.display()
                ))
            })
    }

    pub(super) fn create_directory_durable(path: &Path) -> Result<()> {
        if path.is_dir() {
            return Ok(());
        }
        let parent = path
            .parent()
            .ok_or_else(|| Error::InternalError("Invalid directory path".to_string()))?;
        if !parent.as_os_str().is_empty() {
            Self::create_directory_durable(parent)?;
        }
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(error) => {
                return Err(Error::InternalError(format!(
                    "Failed to create directory {}: {error}",
                    path.display()
                )));
            }
        }
        if !parent.as_os_str().is_empty() {
            Self::sync_directory(parent)?;
        }
        Ok(())
    }

    pub(super) fn recover_publications(root: &Path) -> Result<()> {
        for bucket in fs::read_dir(root).map_err(|error| {
            Error::InternalError(format!("Failed to inspect storage recovery root: {error}"))
        })? {
            let bucket = bucket.map_err(|error| Error::InternalError(error.to_string()))?;
            if bucket
                .file_type()
                .map_err(|error| Error::InternalError(error.to_string()))?
                .is_dir()
                && bucket.path().join(".bucket.name").is_file()
            {
                Self::clean_retired_uploads(&bucket.path())?;
                for object in fs::read_dir(bucket.path())
                    .map_err(|error| Error::InternalError(error.to_string()))?
                {
                    let object = object.map_err(|error| Error::InternalError(error.to_string()))?;
                    if object
                        .file_type()
                        .map_err(|error| Error::InternalError(error.to_string()))?
                        .is_dir()
                        && !object.file_name().to_string_lossy().starts_with('.')
                    {
                        Self::recover_publication(&object.path())?;
                        let versions = object.path().join("versions");
                        if versions.is_dir() {
                            for version in fs::read_dir(versions)
                                .map_err(|error| Error::InternalError(error.to_string()))?
                            {
                                let version = version
                                    .map_err(|error| Error::InternalError(error.to_string()))?;
                                if version
                                    .file_type()
                                    .map_err(|error| Error::InternalError(error.to_string()))?
                                    .is_dir()
                                {
                                    Self::recover_publication(&version.path())?;
                                    Self::remove_empty_recovered_object(&version.path())?;
                                }
                            }
                        }
                        Self::remove_empty_recovered_object(&object.path())?;
                    }
                }
            }
        }
        Ok(())
    }

    fn clean_retired_uploads(bucket: &Path) -> Result<()> {
        let root = bucket.join(".multipart");
        if !root.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(&root).map_err(|error| Error::InternalError(error.to_string()))? {
            let entry = entry.map_err(|error| Error::InternalError(error.to_string()))?;
            if entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(".retired-upload-"))
                .is_some_and(|suffix| Uuid::parse_str(suffix).is_ok())
            {
                fs::remove_dir_all(entry.path())
                    .map_err(|error| Error::InternalError(error.to_string()))?;
                Self::sync_directory(&root)?;
            }
        }
        Ok(())
    }

    fn remove_empty_recovered_object(directory: &Path) -> Result<()> {
        let versions = directory.join("versions");
        if versions.is_dir()
            && fs::read_dir(&versions)
                .map_err(|error| Error::InternalError(error.to_string()))?
                .next()
                .is_none()
        {
            fs::remove_dir(&versions).map_err(|error| Error::InternalError(error.to_string()))?;
            Self::sync_directory(directory)?;
        }
        if fs::read_dir(directory)
            .map_err(|error| Error::InternalError(error.to_string()))?
            .next()
            .is_none()
        {
            fs::remove_dir(directory).map_err(|error| Error::InternalError(error.to_string()))?;
            if let Some(parent) = directory.parent() {
                Self::sync_directory(parent)?;
            }
        }
        Ok(())
    }

    pub(super) fn recover_publication(directory: &Path) -> Result<()> {
        if !directory.exists() {
            return Ok(());
        }
        if directory.join(JOURNAL).exists() {
            Self::apply_publication(directory, None)?;
        }
        Self::clean_uncommitted_stages(directory)
    }

    fn clean_uncommitted_stages(directory: &Path) -> Result<()> {
        for entry in
            fs::read_dir(directory).map_err(|error| Error::InternalError(error.to_string()))?
        {
            let entry = entry.map_err(|error| Error::InternalError(error.to_string()))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name
                .strip_prefix(STAGE_PREFIX)
                .is_some_and(|suffix| Uuid::parse_str(suffix).is_ok())
            {
                fs::remove_dir_all(entry.path()).map_err(|error| {
                    Error::InternalError(format!(
                        "Failed to remove uncommitted publication: {error}"
                    ))
                })?;
                Self::sync_directory(directory)?;
            } else if Self::publication_temp(name) {
                fs::remove_file(entry.path())
                    .map_err(|error| Error::InternalError(error.to_string()))?;
                Self::sync_directory(directory)?;
            }
        }
        Ok(())
    }

    fn publication_temp(name: &str) -> bool {
        [
            ".published-",
            ".copy-",
            ".object.blob.",
            ".object.meta.json.",
            "..publication.json.",
        ]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|suffix| suffix.strip_suffix(".tmp"))
                .is_some_and(|suffix| Uuid::parse_str(suffix).is_ok())
        })
    }

    pub(super) fn publish_pair(
        &self,
        directory: &Path,
        object: &Object,
        payload: ObjectPayload<'_>,
    ) -> Result<()> {
        Self::recover_publication(directory)?;
        Self::create_directory_durable(directory)?;
        #[cfg(test)]
        self.test_phase(super::TestPhase::DirectoryCreated);
        let record = Publication {
            stage: format!("{STAGE_PREFIX}{}", Uuid::new_v4()),
            publish: true,
            clear_current: false,
            remove_versions: if object.version_id.as_deref() == Some("null")
                && directory
                    .parent()
                    .and_then(Path::file_name)
                    .is_none_or(|name| name != "versions")
            {
                vec!["null".to_string()]
            } else {
                Vec::new()
            },
            marker: None,
        };
        let stage = directory.join(&record.stage);
        Self::create_directory_durable(&stage)?;
        match payload {
            ObjectPayload::InMemory => {
                Self::atomic_write(&stage.join("object.blob"), &object.data)?;
            }
            ObjectPayload::Spooled(path) => Self::atomic_move(path, &stage.join("object.blob"))?,
            ObjectPayload::Stored(path) => {
                Self::stage_stored_payload(path, &stage.join("object.blob"))?;
            }
        }
        Self::write_object_metadata(&stage.join("object.meta.json"), object)?;
        self.commit_publication(directory, &record)
    }

    pub(super) fn change_history(
        &self,
        directory: &Path,
        promoted: Option<(&Object, &Path)>,
        remove_versions: Vec<String>,
        marker: Option<Object>,
        clear_current: bool,
    ) -> Result<()> {
        Self::recover_publication(directory)?;
        Self::create_directory_durable(directory)?;
        #[cfg(test)]
        self.test_phase(super::TestPhase::DirectoryCreated);
        let record = Publication {
            stage: format!("{STAGE_PREFIX}{}", Uuid::new_v4()),
            publish: promoted.is_some(),
            clear_current,
            remove_versions,
            marker,
        };
        let stage = directory.join(&record.stage);
        Self::create_directory_durable(&stage)?;
        if let Some((object, path)) = promoted {
            Self::stage_stored_payload(path, &stage.join("object.blob"))?;
            Self::write_object_metadata(&stage.join("object.meta.json"), object)?;
        }
        self.commit_publication(directory, &record)
    }

    fn commit_publication(&self, directory: &Path, record: &Publication) -> Result<()> {
        if let Err(error) = Self::validate_publication(record, &directory.join(&record.stage)) {
            Self::clean_uncommitted_stages(directory)?;
            return Err(error);
        }
        #[cfg(test)]
        self.test_phase(super::TestPhase::PublicationStaged);
        let journal = directory.join(JOURNAL);
        let json =
            serde_json::to_vec(record).map_err(|error| Error::InternalError(error.to_string()))?;
        // The synced journal is the commit decision. Before it, public files
        // are untouched. After it, failures recover this staged generation.
        let outcome = Self::atomic_write(&journal, &json).and_then(|()| {
            #[cfg(test)]
            self.test_phase(super::TestPhase::PublicationCommitted);
            Self::apply_publication(directory, Some(self))
        });
        match outcome {
            Ok(()) => Ok(()),
            Err(error) if journal.exists() => {
                Self::recover_publication(directory).map_err(|recovery| {
                    Error::InternalError(format!(
                        "Publication failed: {error}; recovery required before reads: {recovery}"
                    ))
                })
            }
            Err(error) => {
                Self::clean_uncommitted_stages(directory)?;
                Err(error)
            }
        }
    }

    fn apply_publication(directory: &Path, hooks: Option<&Self>) -> Result<()> {
        let journal = directory.join(JOURNAL);
        let json = fs::read(&journal).map_err(|error| Error::InternalError(error.to_string()))?;
        let record: Publication = serde_json::from_slice(&json).map_err(|error| {
            Error::InternalError(format!("Invalid object publication journal: {error}"))
        })?;
        if record
            .stage
            .strip_prefix(STAGE_PREFIX)
            .is_none_or(|suffix| Uuid::parse_str(suffix).is_err())
        {
            return Err(Error::InternalError(
                "Invalid object publication staging identity".to_string(),
            ));
        }
        let stage = directory.join(&record.stage);
        Self::validate_publication(&record, &stage)?;
        if record.publish {
            Self::publish_staged_file(
                &stage.join("object.blob"),
                &directory.join("object.blob"),
                hooks,
            )?;
            #[cfg(test)]
            if let Some(storage) = hooks {
                storage.test_phase(
                    if directory
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name == "versions")
                    {
                        super::TestPhase::VersionBodyPublished
                    } else {
                        super::TestPhase::BodyPublished
                    },
                );
            }
            Self::publish_staged_file(
                &stage.join("object.meta.json"),
                &directory.join("object.meta.json"),
                hooks,
            )?;
            #[cfg(test)]
            if let Some(storage) = hooks {
                storage.test_phase(
                    if directory
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name == "versions")
                    {
                        super::TestPhase::VersionMetadataPublished
                    } else {
                        super::TestPhase::MetadataPublished
                    },
                );
            }
        }
        Self::apply_history_changes(directory, &record, hooks)?;
        fs::remove_file(&journal).map_err(|error| {
            Error::InternalError(format!("Failed to retire publication journal: {error}"))
        })?;
        Self::sync_directory(directory)?;
        fs::remove_dir_all(&stage).map_err(|error| {
            Error::InternalError(format!("Failed to clean committed publication: {error}"))
        })?;
        Self::sync_directory(directory)?;
        #[cfg(test)]
        if let Some(storage) = hooks {
            storage.test_phase(super::TestPhase::PublicationCleaned);
        }
        Ok(())
    }

    fn validate_publication(record: &Publication, stage: &Path) -> Result<()> {
        if record.publish && (record.clear_current || record.marker.is_some()) {
            return Err(Error::InternalError(
                "Conflicting publication actions".to_string(),
            ));
        }
        for id in &record.remove_versions {
            Self::validate_version_id(id)?;
        }
        if let Some(marker) = &record.marker {
            Self::validate_version_id(marker.version_id.as_deref().ok_or_else(|| {
                Error::InternalError("Missing delete marker identity".to_string())
            })?)?;
            if marker.size != 0 || !record.clear_current {
                return Err(Error::InternalError(
                    "Invalid delete marker publication".to_string(),
                ));
            }
        }
        if record.publish {
            let json = fs::read(stage.join("object.meta.json")).map_err(|error| {
                Error::InternalError(format!("Cannot recover staged metadata: {error}"))
            })?;
            let object: Object = serde_json::from_slice(&json).map_err(|error| {
                Error::InternalError(format!("Invalid staged metadata: {error}"))
            })?;
            let size = fs::metadata(stage.join("object.blob"))
                .map_err(|error| {
                    Error::InternalError(format!("Cannot recover staged body: {error}"))
                })?
                .len();
            if size != object.size {
                return Err(Error::InternalError(
                    "Staged object body and metadata sizes disagree".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn stage_stored_payload(source: &Path, destination: &Path) -> Result<()> {
        let parent = destination.parent().ok_or_else(|| {
            Error::InternalError("Invalid stored payload staging path".to_string())
        })?;
        // Public bodies are replaced by rename, never edited in place. A hard
        // link preserves the selected generation without materializing bytes.
        if fs::hard_link(source, destination).is_err() {
            Self::atomic_copy(source, destination)?;
        }
        fs::File::open(destination)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                Error::InternalError(format!("Failed to sync stored generation: {error}"))
            })?;
        Self::sync_directory(parent)
    }

    fn publish_staged_file(source: &Path, destination: &Path, hooks: Option<&Self>) -> Result<()> {
        let parent = destination
            .parent()
            .ok_or_else(|| Error::InternalError("Invalid publication path".to_string()))?;
        let temp = parent.join(format!(".published-{}.tmp", Uuid::new_v4()));
        if fs::hard_link(source, &temp).is_err() {
            // Filesystems without hard links retain the same commit semantics;
            // the fallback uses a synced private copy, never the public path.
            Self::atomic_copy(source, &temp)?;
        }
        #[cfg(test)]
        if let Some(storage) = hooks {
            let version = parent
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "versions");
            let metadata = destination
                .file_name()
                .is_some_and(|name| name == "object.meta.json");
            storage.test_phase(match (version, metadata) {
                (false, false) => super::TestPhase::BodyPrepared,
                (false, true) => super::TestPhase::MetadataPrepared,
                (true, false) => super::TestPhase::VersionBodyPrepared,
                (true, true) => super::TestPhase::VersionMetadataPrepared,
            });
        }
        #[cfg(not(test))]
        let _ = hooks;
        fs::rename(&temp, destination).map_err(|error| {
            Error::InternalError(format!(
                "Failed to publish staged object generation: {error}"
            ))
        })?;
        Self::sync_directory(parent)
    }

    fn apply_history_changes(
        directory: &Path,
        record: &Publication,
        hooks: Option<&Self>,
    ) -> Result<()> {
        for id in &record.remove_versions {
            Self::validate_version_id(id)?;
            let path = directory.join("versions").join(id);
            if path.exists() {
                let body = path.join("object.blob");
                if body.exists() {
                    fs::remove_file(body)
                        .map_err(|error| Error::InternalError(error.to_string()))?;
                    Self::sync_directory(&path)?;
                }
                #[cfg(test)]
                if let Some(storage) = hooks {
                    storage.test_phase(super::TestPhase::VersionRetiring);
                }
                fs::remove_dir_all(path).map_err(|error| {
                    Error::InternalError(format!("Failed to retire historical version: {error}"))
                })?;
                Self::sync_directory(&directory.join("versions"))?;
            }
            #[cfg(test)]
            if let Some(storage) = hooks {
                storage.test_phase(super::TestPhase::VersionDeleted);
            }
        }
        if let Some(marker) = &record.marker {
            let id = marker.version_id.as_deref().ok_or_else(|| {
                Error::InternalError("Missing marker version identity".to_string())
            })?;
            Self::validate_version_id(id)?;
            let path = directory.join("versions").join(id);
            Self::create_directory_durable(&path)?;
            Self::atomic_write(&path.join("object.blob"), b"")?;
            #[cfg(test)]
            if let Some(storage) = hooks {
                storage.test_phase(super::TestPhase::MarkerBodyPublished);
            }
            Self::write_object_metadata(&path.join("object.meta.json"), marker)?;
            #[cfg(test)]
            if let Some(storage) = hooks {
                storage.test_phase(super::TestPhase::MarkerMetadataPublished);
            }
            #[cfg(test)]
            if let Some(storage) = hooks {
                storage.test_phase(super::TestPhase::VersionPublished);
            }
        }
        if record.clear_current {
            for name in ["object.blob", "object.meta.json"] {
                let path = directory.join(name);
                if path.exists() {
                    fs::remove_file(path)
                        .map_err(|error| Error::InternalError(error.to_string()))?;
                    Self::sync_directory(directory)?;
                    #[cfg(test)]
                    if name == "object.blob" {
                        if let Some(storage) = hooks {
                            storage.test_phase(super::TestPhase::CurrentBodyRemoved);
                        }
                    }
                }
            }
            #[cfg(test)]
            if let Some(storage) = hooks {
                storage.test_phase(super::TestPhase::CurrentRemoved);
            }
        }
        #[cfg(not(test))]
        let _ = hooks;
        Ok(())
    }
}
