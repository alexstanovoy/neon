//!
//! Timeline repository implementation that keeps old data in files on disk, and
//! the recent changes in memory. See tenant/*_layer.rs files.
//! The functions here are responsible for locating the correct layer for the
//! get/put call, walking back the timeline branching history as needed.
//!
//! The files are stored in the .neon/tenants/<tenant_id>/timelines/<timeline_id>
//! directory. See docs/pageserver-storage.md for how the files are managed.
//! In addition to the layer files, there is a metadata file in the same
//! directory that contains information about the timeline, in particular its
//! parent timeline, and the last LSN that has been written to disk.
//!

use anyhow::{bail, Context};
use pageserver_api::models::TimelineState;
use tokio::sync::watch;
use tracing::*;
use utils::crashsafe::path_with_suffix_extension;

use std::cmp::min;
use std::collections::hash_map::Entry;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::ops::Bound::Included;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::MutexGuard;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use self::metadata::TimelineMetadata;
use crate::config::PageServerConf;
use crate::import_datadir;
use crate::metrics::{remove_tenant_metrics, STORAGE_TIME};
use crate::repository::GcResult;
use crate::storage_sync::index::RemoteIndex;
use crate::task_mgr;
use crate::tenant_config::TenantConfOpt;
use crate::virtual_file::VirtualFile;
use crate::walredo::WalRedoManager;
use crate::{CheckpointConfig, TEMP_FILE_SUFFIX};
pub use pageserver_api::models::TenantState;

use toml_edit;
use utils::{
    crashsafe,
    id::{TenantId, TimelineId},
    lsn::{Lsn, RecordLsn},
};

mod blob_io;
pub mod block_io;
mod delta_layer;
mod disk_btree;
pub(crate) mod ephemeral_file;
pub mod filename;
mod image_layer;
mod inmemory_layer;
pub mod layer_map;

pub mod metadata;
mod par_fsync;
pub mod storage_layer;

mod timeline;

use storage_layer::Layer;

pub use timeline::Timeline;

// re-export this function so that page_cache.rs can use it.
pub use crate::tenant::ephemeral_file::writeback as writeback_ephemeral_file;

// re-export for use in storage_sync.rs
pub use crate::tenant::metadata::save_metadata;

// re-export for use in walreceiver
pub use crate::tenant::timeline::WalReceiverInfo;

/// Parts of the `.neon/tenants/<tenant_id>/timelines/<timeline_id>` directory prefix.
pub const TIMELINES_SEGMENT_NAME: &str = "timelines";

///
/// Tenant consists of multiple timelines. Keep them in a hash table.
///
pub struct Tenant {
    // Global pageserver config parameters
    pub conf: &'static PageServerConf,

    state: watch::Sender<TenantState>,

    // Overridden tenant-specific config parameters.
    // We keep TenantConfOpt sturct here to preserve the information
    // about parameters that are not set.
    // This is necessary to allow global config updates.
    tenant_conf: Arc<RwLock<TenantConfOpt>>,

    tenant_id: TenantId,
    timelines: Mutex<HashMap<TimelineId, Arc<Timeline>>>,
    // This mutex prevents creation of new timelines during GC.
    // Adding yet another mutex (in addition to `timelines`) is needed because holding
    // `timelines` mutex during all GC iteration (especially with enforced checkpoint)
    // may block for a long time `get_timeline`, `get_timelines_state`,... and other operations
    // with timelines, which in turn may cause dropping replication connection, expiration of wait_for_lsn
    // timeout...
    gc_cs: Mutex<()>,
    walredo_mgr: Arc<dyn WalRedoManager + Send + Sync>,

    // provides access to timeline data sitting in the remote storage
    // supposed to be used for retrieval of remote consistent lsn in walreceiver
    remote_index: RemoteIndex,

    /// Makes every timeline to backup their files to remote storage.
    upload_layers: bool,
}

/// A timeline with some of its files on disk, being initialized.
/// This struct ensures the atomicity of the timeline init: it's either properly created and inserted into pageserver's memory, or
/// its local files are removed. In the worst case of a crash, an uninit mark file is left behind, which causes the directory
/// to be removed on next restart.
///
/// The caller is responsible for proper timeline data filling before the final init.
#[must_use]
pub struct UninitializedTimeline<'t> {
    owning_tenant: &'t Tenant,
    timeline_id: TimelineId,
    raw_timeline: Option<(Timeline, TimelineUninitMark)>,
}

/// An uninit mark file, created along the timeline dir to ensure the timeline either gets fully initialized and loaded into pageserver's memory,
/// or gets removed eventually.
///
/// XXX: it's important to create it near the timeline dir, not inside it to ensure timeline dir gets removed first.
#[must_use]
struct TimelineUninitMark {
    uninit_mark_deleted: bool,
    uninit_mark_path: PathBuf,
    timeline_path: PathBuf,
}

impl UninitializedTimeline<'_> {
    /// Ensures timeline data is valid, loads it into pageserver's memory and removes uninit mark file on success.
    pub fn initialize(self) -> anyhow::Result<Arc<Timeline>> {
        let mut timelines = self.owning_tenant.timelines.lock().unwrap();
        self.initialize_with_lock(&mut timelines, true)
    }

    fn initialize_with_lock(
        mut self,
        timelines: &mut HashMap<TimelineId, Arc<Timeline>>,
        load_layer_map: bool,
    ) -> anyhow::Result<Arc<Timeline>> {
        let timeline_id = self.timeline_id;
        let tenant_id = self.owning_tenant.tenant_id;

        let (new_timeline, uninit_mark) = self.raw_timeline.take().with_context(|| {
            format!("No timeline for initalization found for {tenant_id}/{timeline_id}")
        })?;
        let new_timeline = Arc::new(new_timeline);

        let new_disk_consistent_lsn = new_timeline.get_disk_consistent_lsn();
        // TODO it would be good to ensure that, but apparently a lot of our testing is dependend on that at least
        // ensure!(new_disk_consistent_lsn.is_valid(),
        //     "Timeline {tenant_id}/{timeline_id} has invalid disk_consistent_lsn and cannot be initialized");

        match timelines.entry(timeline_id) {
            Entry::Occupied(_) => anyhow::bail!(
                "Found freshly initialized timeline {tenant_id}/{timeline_id} in the tenant map"
            ),
            Entry::Vacant(v) => {
                if load_layer_map {
                    new_timeline
                        .load_layer_map(new_disk_consistent_lsn)
                        .with_context(|| {
                            format!(
                                "Failed to load layermap for timeline {tenant_id}/{timeline_id}"
                            )
                        })?;
                }
                uninit_mark.remove_uninit_mark().with_context(|| {
                    format!(
                        "Failed to remove uninit mark file for timeline {tenant_id}/{timeline_id}"
                    )
                })?;
                new_timeline.set_state(TimelineState::Active);
                v.insert(Arc::clone(&new_timeline));
                new_timeline.launch_wal_receiver();
            }
        }

        Ok(new_timeline)
    }

    /// Prepares timeline data by loading it from the basebackup archive.
    pub fn import_basebackup_from_tar(
        &self,
        reader: impl std::io::Read,
        base_lsn: Lsn,
    ) -> anyhow::Result<()> {
        let raw_timeline = self.raw_timeline()?;
        import_datadir::import_basebackup_from_tar(raw_timeline, reader, base_lsn).with_context(
            || {
                format!(
                    "Failed to import basebackup for timeline {}/{}",
                    self.owning_tenant.tenant_id, self.timeline_id
                )
            },
        )?;

        fail::fail_point!("before-checkpoint-new-timeline", |_| {
            bail!("failpoint before-checkpoint-new-timeline");
        });

        raw_timeline
            .checkpoint(CheckpointConfig::Flush)
            .with_context(|| {
                format!(
                    "Failed to checkpoint after basebackup import for timeline {}/{}",
                    self.owning_tenant.tenant_id, self.timeline_id
                )
            })?;
        Ok(())
    }

    fn raw_timeline(&self) -> anyhow::Result<&Timeline> {
        Ok(&self
            .raw_timeline
            .as_ref()
            .with_context(|| {
                format!(
                    "No raw timeline {}/{} found",
                    self.owning_tenant.tenant_id, self.timeline_id
                )
            })?
            .0)
    }
}

impl Drop for UninitializedTimeline<'_> {
    fn drop(&mut self) {
        if let Some((_, uninit_mark)) = self.raw_timeline.take() {
            let _entered = info_span!("drop_uninitialized_timeline", tenant = %self.owning_tenant.tenant_id, timeline = %self.timeline_id).entered();
            error!("Timeline got dropped without initializing, cleaning its files");
            cleanup_timeline_directory(uninit_mark);
        }
    }
}

fn cleanup_timeline_directory(uninit_mark: TimelineUninitMark) {
    let timeline_path = &uninit_mark.timeline_path;
    match ignore_absent_files(|| fs::remove_dir_all(timeline_path)) {
        Ok(()) => {
            info!("Timeline dir {timeline_path:?} removed successfully, removing the uninit mark")
        }
        Err(e) => {
            error!("Failed to clean up uninitialized timeline directory {timeline_path:?}: {e:?}")
        }
    }
    drop(uninit_mark); // mark handles its deletion on drop, gets retained if timeline dir exists
}

impl TimelineUninitMark {
    /// Useful for initializing timelines, existing on disk after the restart.
    pub fn dummy() -> Self {
        Self {
            uninit_mark_deleted: true,
            uninit_mark_path: PathBuf::new(),
            timeline_path: PathBuf::new(),
        }
    }

    fn new(uninit_mark_path: PathBuf, timeline_path: PathBuf) -> Self {
        Self {
            uninit_mark_deleted: false,
            uninit_mark_path,
            timeline_path,
        }
    }

    fn remove_uninit_mark(mut self) -> anyhow::Result<()> {
        if !self.uninit_mark_deleted {
            self.delete_mark_file_if_present()?;
        }

        Ok(())
    }

    fn delete_mark_file_if_present(&mut self) -> anyhow::Result<()> {
        let uninit_mark_file = &self.uninit_mark_path;
        let uninit_mark_parent = uninit_mark_file
            .parent()
            .with_context(|| format!("Uninit mark file {uninit_mark_file:?} has no parent"))?;
        ignore_absent_files(|| fs::remove_file(&uninit_mark_file)).with_context(|| {
            format!("Failed to remove uninit mark file at path {uninit_mark_file:?}")
        })?;
        crashsafe::fsync(uninit_mark_parent).context("Failed to fsync uninit mark parent")?;
        self.uninit_mark_deleted = true;

        Ok(())
    }
}

impl Drop for TimelineUninitMark {
    fn drop(&mut self) {
        if !self.uninit_mark_deleted {
            if self.timeline_path.exists() {
                error!(
                    "Uninit mark {} is not removed, timeline {} stays uninitialized",
                    self.uninit_mark_path.display(),
                    self.timeline_path.display()
                )
            } else {
                // unblock later timeline creation attempts
                warn!(
                    "Removing intermediate uninit mark file {}",
                    self.uninit_mark_path.display()
                );
                if let Err(e) = self.delete_mark_file_if_present() {
                    error!("Failed to remove the uninit mark file: {e}")
                }
            }
        }
    }
}

/// A repository corresponds to one .neon directory. One repository holds multiple
/// timelines, forked off from the same initial call to 'initdb'.
impl Tenant {
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Get Timeline handle for given Neon timeline ID.
    /// This function is idempotent. It doesn't change internal state in any way.
    pub fn get_timeline(
        &self,
        timeline_id: TimelineId,
        active_only: bool,
    ) -> anyhow::Result<Arc<Timeline>> {
        let timelines_accessor = self.timelines.lock().unwrap();
        let timeline = timelines_accessor.get(&timeline_id).with_context(|| {
            format!("Timeline {}/{} was not found", self.tenant_id, timeline_id)
        })?;

        if active_only && !timeline.is_active() {
            anyhow::bail!(
                "Timeline {}/{} is not active, state: {:?}",
                self.tenant_id,
                timeline_id,
                timeline.current_state()
            )
        } else {
            Ok(Arc::clone(timeline))
        }
    }

    /// Lists timelines the tenant contains.
    /// Up to tenant's implementation to omit certain timelines that ar not considered ready for use.
    pub fn list_timelines(&self) -> Vec<Arc<Timeline>> {
        self.timelines
            .lock()
            .unwrap()
            .values()
            .map(Arc::clone)
            .collect()
    }

    /// This is used to create the initial 'main' timeline during bootstrapping,
    /// or when importing a new base backup. The caller is expected to load an
    /// initial image of the datadir to the new timeline after this.
    pub fn create_empty_timeline(
        &self,
        new_timeline_id: TimelineId,
        initdb_lsn: Lsn,
        pg_version: u32,
    ) -> anyhow::Result<UninitializedTimeline> {
        anyhow::ensure!(
            self.is_active(),
            "Cannot create empty timelines on inactive tenant"
        );

        let timelines = self.timelines.lock().unwrap();
        let timeline_uninit_mark = self.create_timeline_uninit_mark(new_timeline_id, &timelines)?;
        drop(timelines);

        let new_metadata = TimelineMetadata::new(
            Lsn(0),
            None,
            None,
            Lsn(0),
            initdb_lsn,
            initdb_lsn,
            pg_version,
        );
        self.prepare_timeline(
            new_timeline_id,
            new_metadata,
            timeline_uninit_mark,
            true,
            None,
        )
    }

    /// Create a new timeline.
    ///
    /// Returns the new timeline ID and reference to its Timeline object.
    ///
    /// If the caller specified the timeline ID to use (`new_timeline_id`), and timeline with
    /// the same timeline ID already exists, returns None. If `new_timeline_id` is not given,
    /// a new unique ID is generated.
    pub async fn create_timeline(
        &self,
        new_timeline_id: Option<TimelineId>,
        ancestor_timeline_id: Option<TimelineId>,
        mut ancestor_start_lsn: Option<Lsn>,
        pg_version: u32,
    ) -> anyhow::Result<Option<Arc<Timeline>>> {
        anyhow::ensure!(
            self.is_active(),
            "Cannot create timelines on inactive tenant"
        );

        let new_timeline_id = new_timeline_id.unwrap_or_else(TimelineId::generate);

        if self.get_timeline(new_timeline_id, false).is_ok() {
            debug!("timeline {new_timeline_id} already exists");
            return Ok(None);
        }

        let loaded_timeline = match ancestor_timeline_id {
            Some(ancestor_timeline_id) => {
                let ancestor_timeline = self
                    .get_timeline(ancestor_timeline_id, false)
                    .context("Cannot branch off the timeline that's not present in pageserver")?;

                if let Some(lsn) = ancestor_start_lsn.as_mut() {
                    // Wait for the WAL to arrive and be processed on the parent branch up
                    // to the requested branch point. The repository code itself doesn't
                    // require it, but if we start to receive WAL on the new timeline,
                    // decoding the new WAL might need to look up previous pages, relation
                    // sizes etc. and that would get confused if the previous page versions
                    // are not in the repository yet.
                    *lsn = lsn.align();
                    ancestor_timeline.wait_lsn(*lsn).await?;

                    let ancestor_ancestor_lsn = ancestor_timeline.get_ancestor_lsn();
                    if ancestor_ancestor_lsn > *lsn {
                        // can we safely just branch from the ancestor instead?
                        bail!(
                            "invalid start lsn {} for ancestor timeline {}: less than timeline ancestor lsn {}",
                            lsn,
                            ancestor_timeline_id,
                            ancestor_ancestor_lsn,
                        );
                    }
                }

                self.branch_timeline(ancestor_timeline_id, new_timeline_id, ancestor_start_lsn)?
            }
            None => self.bootstrap_timeline(new_timeline_id, pg_version)?,
        };

        // Have added new timeline into the tenant, now its background tasks are needed.
        self.activate(true);

        Ok(Some(loaded_timeline))
    }

    /// perform one garbage collection iteration, removing old data files from disk.
    /// this function is periodically called by gc task.
    /// also it can be explicitly requested through page server api 'do_gc' command.
    ///
    /// 'target_timeline_id' specifies the timeline to GC, or None for all.
    /// `horizon` specifies delta from last lsn to preserve all object versions (pitr interval).
    /// `checkpoint_before_gc` parameter is used to force compaction of storage before GC
    /// to make tests more deterministic.
    /// TODO Do we still need it or we can call checkpoint explicitly in tests where needed?
    pub fn gc_iteration(
        &self,
        target_timeline_id: Option<TimelineId>,
        horizon: u64,
        pitr: Duration,
        checkpoint_before_gc: bool,
    ) -> anyhow::Result<GcResult> {
        anyhow::ensure!(
            self.is_active(),
            "Cannot run GC iteration on inactive tenant"
        );

        let timeline_str = target_timeline_id
            .map(|x| x.to_string())
            .unwrap_or_else(|| "-".to_string());

        STORAGE_TIME
            .with_label_values(&["gc", &self.tenant_id.to_string(), &timeline_str])
            .observe_closure_duration(|| {
                self.gc_iteration_internal(target_timeline_id, horizon, pitr, checkpoint_before_gc)
            })
    }

    /// Perform one compaction iteration.
    /// This function is periodically called by compactor task.
    /// Also it can be explicitly requested per timeline through page server
    /// api's 'compact' command.
    pub fn compaction_iteration(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_active(),
            "Cannot run compaction iteration on inactive tenant"
        );

        // Scan through the hashmap and collect a list of all the timelines,
        // while holding the lock. Then drop the lock and actually perform the
        // compactions.  We don't want to block everything else while the
        // compaction runs.
        let timelines = self.timelines.lock().unwrap();
        let timelines_to_compact = timelines
            .iter()
            .filter(|(_, timeline)| timeline.is_active())
            .map(|(timeline_id, timeline)| (*timeline_id, timeline.clone()))
            .collect::<Vec<_>>();
        drop(timelines);

        for (timeline_id, timeline) in &timelines_to_compact {
            let _entered = info_span!("compact_timeline", timeline = %timeline_id).entered();
            timeline.compact()?;
        }

        Ok(())
    }

    /// Flush all in-memory data to disk.
    ///
    /// Used at graceful shutdown.
    ///
    pub fn checkpoint(&self) -> anyhow::Result<()> {
        // Scan through the hashmap and collect a list of all the timelines,
        // while holding the lock. Then drop the lock and actually perform the
        // checkpoints. We don't want to block everything else while the
        // checkpoint runs.
        let timelines = self.timelines.lock().unwrap();
        let timelines_to_checkpoint = timelines
            .iter()
            .map(|(timeline_id, timeline)| (*timeline_id, Arc::clone(timeline)))
            .collect::<Vec<_>>();
        drop(timelines);

        for (timeline_id, timeline) in &timelines_to_checkpoint {
            let _entered =
                info_span!("checkpoint", timeline = %timeline_id, tenant = %self.tenant_id)
                    .entered();
            timeline.checkpoint(CheckpointConfig::Flush)?;
        }

        Ok(())
    }

    /// Removes timeline-related in-memory data
    pub fn delete_timeline(&self, timeline_id: TimelineId) -> anyhow::Result<()> {
        // in order to be retriable detach needs to be idempotent
        // (or at least to a point that each time the detach is called it can make progress)
        let mut timelines = self.timelines.lock().unwrap();

        // Ensure that there are no child timelines **attached to that pageserver**,
        // because detach removes files, which will break child branches
        let children_exist = timelines
            .iter()
            .any(|(_, entry)| entry.get_ancestor_timeline_id() == Some(timeline_id));

        anyhow::ensure!(
            !children_exist,
            "Cannot delete timeline which has child timelines"
        );
        let timeline_entry = match timelines.entry(timeline_id) {
            Entry::Occupied(e) => e,
            Entry::Vacant(_) => bail!("timeline not found"),
        };

        let timeline = timeline_entry.get();
        timeline.set_state(TimelineState::Paused);

        let layer_removal_guard = timeline.layer_removal_guard()?;

        let local_timeline_directory = self.conf.timeline_path(&timeline_id, &self.tenant_id);
        std::fs::remove_dir_all(&local_timeline_directory).with_context(|| {
            format!(
                "Failed to remove local timeline directory '{}'",
                local_timeline_directory.display()
            )
        })?;
        info!("detach removed files");

        drop(layer_removal_guard);
        timeline_entry.remove();

        Ok(())
    }

    /// Allows to retrieve remote timeline index from the tenant. Used in walreceiver to grab remote consistent lsn.
    pub fn get_remote_index(&self) -> &RemoteIndex {
        &self.remote_index
    }

    pub fn current_state(&self) -> TenantState {
        *self.state.borrow()
    }

    pub fn is_active(&self) -> bool {
        matches!(self.current_state(), TenantState::Active { .. })
    }

    pub fn should_run_tasks(&self) -> bool {
        matches!(
            self.current_state(),
            TenantState::Active {
                background_jobs_running: true
            }
        )
    }

    /// Changes tenant status to active, if it was not broken before.
    /// Otherwise, ignores the state change, logging an error.
    pub fn activate(&self, enable_background_jobs: bool) {
        self.set_state(TenantState::Active {
            background_jobs_running: enable_background_jobs,
        });
    }

    pub fn set_state(&self, new_state: TenantState) {
        match (self.current_state(), new_state) {
            (equal_state_1, equal_state_2) if equal_state_1 == equal_state_2 => {
                debug!("Ignoring new state, equal to the existing one: {equal_state_2:?}");
            }
            (TenantState::Broken, _) => {
                error!("Ignoring state update {new_state:?} for broken tenant");
            }
            (_, new_state) => {
                self.state.send_replace(new_state);

                let timelines_accessor = self.timelines.lock().unwrap();
                let not_broken_timelines = timelines_accessor
                    .values()
                    .filter(|timeline| timeline.current_state() != TimelineState::Broken);
                match new_state {
                    TenantState::Active {
                        background_jobs_running,
                    } => {
                        if background_jobs_running {
                            // Spawn gc and compaction loops. The loops will shut themselves
                            // down when they notice that the tenant is inactive.
                            crate::tenant_tasks::start_background_loops(self.tenant_id);
                        }

                        for timeline in not_broken_timelines {
                            timeline.set_state(TimelineState::Active);
                        }
                    }
                    TenantState::Paused | TenantState::Broken => {
                        for timeline in not_broken_timelines {
                            timeline.set_state(TimelineState::Suspended);
                        }
                    }
                }
            }
        }
    }

    pub fn subscribe_for_state_updates(&self) -> watch::Receiver<TenantState> {
        self.state.subscribe()
    }
}

/// Given a Vec of timelines and their ancestors (timeline_id, ancestor_id),
/// perform a topological sort, so that the parent of each timeline comes
/// before the children.
fn tree_sort_timelines(
    timelines: HashMap<TimelineId, TimelineMetadata>,
) -> anyhow::Result<Vec<(TimelineId, TimelineMetadata)>> {
    let mut result = Vec::with_capacity(timelines.len());

    let mut now = Vec::with_capacity(timelines.len());
    // (ancestor, children)
    let mut later: HashMap<TimelineId, Vec<(TimelineId, TimelineMetadata)>> =
        HashMap::with_capacity(timelines.len());

    for (timeline_id, metadata) in timelines {
        if let Some(ancestor_id) = metadata.ancestor_timeline() {
            let children = later.entry(ancestor_id).or_default();
            children.push((timeline_id, metadata));
        } else {
            now.push((timeline_id, metadata));
        }
    }

    while let Some((timeline_id, metadata)) = now.pop() {
        result.push((timeline_id, metadata));
        // All children of this can be loaded now
        if let Some(mut children) = later.remove(&timeline_id) {
            now.append(&mut children);
        }
    }

    // All timelines should be visited now. Unless there were timelines with missing ancestors.
    if !later.is_empty() {
        for (missing_id, orphan_ids) in later {
            for (orphan_id, _) in orphan_ids {
                error!("could not load timeline {orphan_id} because its ancestor timeline {missing_id} could not be loaded");
            }
        }
        bail!("could not load tenant because some timelines are missing ancestors");
    }

    Ok(result)
}

/// Private functions
impl Tenant {
    pub fn get_checkpoint_distance(&self) -> u64 {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .checkpoint_distance
            .unwrap_or(self.conf.default_tenant_conf.checkpoint_distance)
    }

    pub fn get_checkpoint_timeout(&self) -> Duration {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .checkpoint_timeout
            .unwrap_or(self.conf.default_tenant_conf.checkpoint_timeout)
    }

    pub fn get_compaction_target_size(&self) -> u64 {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .compaction_target_size
            .unwrap_or(self.conf.default_tenant_conf.compaction_target_size)
    }

    pub fn get_compaction_period(&self) -> Duration {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .compaction_period
            .unwrap_or(self.conf.default_tenant_conf.compaction_period)
    }

    pub fn get_compaction_threshold(&self) -> usize {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .compaction_threshold
            .unwrap_or(self.conf.default_tenant_conf.compaction_threshold)
    }

    pub fn get_gc_horizon(&self) -> u64 {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .gc_horizon
            .unwrap_or(self.conf.default_tenant_conf.gc_horizon)
    }

    pub fn get_gc_period(&self) -> Duration {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .gc_period
            .unwrap_or(self.conf.default_tenant_conf.gc_period)
    }

    pub fn get_image_creation_threshold(&self) -> usize {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .image_creation_threshold
            .unwrap_or(self.conf.default_tenant_conf.image_creation_threshold)
    }

    pub fn get_pitr_interval(&self) -> Duration {
        let tenant_conf = self.tenant_conf.read().unwrap();
        tenant_conf
            .pitr_interval
            .unwrap_or(self.conf.default_tenant_conf.pitr_interval)
    }

    pub fn update_tenant_config(&self, new_tenant_conf: TenantConfOpt) {
        self.tenant_conf.write().unwrap().update(&new_tenant_conf);
    }

    fn create_timeline_data(
        &self,
        new_timeline_id: TimelineId,
        new_metadata: TimelineMetadata,
        ancestor: Option<Arc<Timeline>>,
    ) -> anyhow::Result<Timeline> {
        if let Some(ancestor_timeline_id) = new_metadata.ancestor_timeline() {
            anyhow::ensure!(
                ancestor.is_some(),
                "Timeline's {new_timeline_id} ancestor {ancestor_timeline_id} was not found"
            )
        }

        let pg_version = new_metadata.pg_version();
        Ok(Timeline::new(
            self.conf,
            Arc::clone(&self.tenant_conf),
            new_metadata,
            ancestor,
            new_timeline_id,
            self.tenant_id,
            Arc::clone(&self.walredo_mgr),
            self.upload_layers,
            pg_version,
        ))
    }

    pub(super) fn new(
        conf: &'static PageServerConf,
        tenant_conf: TenantConfOpt,
        walredo_mgr: Arc<dyn WalRedoManager + Send + Sync>,
        tenant_id: TenantId,
        remote_index: RemoteIndex,
        upload_layers: bool,
    ) -> Tenant {
        let (state, _) = watch::channel(TenantState::Paused);
        Tenant {
            tenant_id,
            conf,
            tenant_conf: Arc::new(RwLock::new(tenant_conf)),
            timelines: Mutex::new(HashMap::new()),
            gc_cs: Mutex::new(()),
            walredo_mgr,
            remote_index,
            upload_layers,
            state,
        }
    }

    /// Locate and load config
    pub(super) fn load_tenant_config(
        conf: &'static PageServerConf,
        tenant_id: TenantId,
    ) -> anyhow::Result<TenantConfOpt> {
        let target_config_path = conf.tenant_config_path(tenant_id);
        let target_config_display = target_config_path.display();

        info!("loading tenantconf from {target_config_display}");

        // FIXME If the config file is not found, assume that we're attaching
        // a detached tenant and config is passed via attach command.
        // https://github.com/neondatabase/neon/issues/1555
        if !target_config_path.exists() {
            info!("tenant config not found in {target_config_display}");
            return Ok(TenantConfOpt::default());
        }

        // load and parse file
        let config = fs::read_to_string(&target_config_path).with_context(|| {
            format!("Failed to load config from path '{target_config_display}'")
        })?;

        let toml = config.parse::<toml_edit::Document>().with_context(|| {
            format!("Failed to parse config from file '{target_config_display}' as toml file")
        })?;

        let mut tenant_conf = TenantConfOpt::default();
        for (key, item) in toml.iter() {
            match key {
                "tenant_config" => {
                    tenant_conf = PageServerConf::parse_toml_tenant_conf(item).with_context(|| {
                        format!("Failed to parse config from file '{target_config_display}' as pageserver config")
                    })?;
                }
                _ => bail!("config file {target_config_display} has unrecognized pageserver option '{key}'"),

            }
        }

        Ok(tenant_conf)
    }

    pub(super) fn persist_tenant_config(
        target_config_path: &Path,
        tenant_conf: TenantConfOpt,
        first_save: bool,
    ) -> anyhow::Result<()> {
        let _enter = info_span!("saving tenantconf").entered();
        info!("persisting tenantconf to {}", target_config_path.display());

        // TODO this will prepend comments endlessly
        let mut conf_content = r#"# This file contains a specific per-tenant's config.
#  It is read in case of pageserver restart.

[tenant_config]
"#
        .to_string();

        // Convert the config to a toml file.
        conf_content += &toml_edit::easy::to_string(&tenant_conf)?;

        let mut target_config_file = VirtualFile::open_with_options(
            target_config_path,
            OpenOptions::new().write(true).create_new(first_save),
        )?;

        target_config_file
            .write(conf_content.as_bytes())
            .context("Failed to write toml bytes into file")
            .and_then(|_| {
                target_config_file
                    .sync_all()
                    .context("Faile to fsync config file")
            })
            .with_context(|| {
                format!(
                    "Failed to write config file into path '{}'",
                    target_config_path.display()
                )
            })?;

        // fsync the parent directory to ensure the directory entry is durable
        if first_save {
            target_config_path
                .parent()
                .context("Config file does not have a parent")
                .and_then(|target_config_parent| {
                    File::open(target_config_parent).context("Failed to open config parent")
                })
                .and_then(|tenant_dir| {
                    tenant_dir
                        .sync_all()
                        .context("Failed to fsync config parent")
                })
                .with_context(|| {
                    format!(
                        "Failed to fsync on first save for config {}",
                        target_config_path.display()
                    )
                })?;
        }

        Ok(())
    }

    //
    // How garbage collection works:
    //
    //                    +--bar------------->
    //                   /
    //             +----+-----foo---------------->
    //            /
    // ----main--+-------------------------->
    //                \
    //                 +-----baz-------->
    //
    //
    // 1. Grab 'gc_cs' mutex to prevent new timelines from being created
    // 2. Scan all timelines, and on each timeline, make note of the
    //    all the points where other timelines have been branched off.
    //    We will refrain from removing page versions at those LSNs.
    // 3. For each timeline, scan all layer files on the timeline.
    //    Remove all files for which a newer file exists and which
    //    don't cover any branch point LSNs.
    //
    // TODO:
    // - if a relation has a non-incremental persistent layer on a child branch, then we
    //   don't need to keep that in the parent anymore. But currently
    //   we do.
    fn gc_iteration_internal(
        &self,
        target_timeline_id: Option<TimelineId>,
        horizon: u64,
        pitr: Duration,
        checkpoint_before_gc: bool,
    ) -> anyhow::Result<GcResult> {
        let mut totals: GcResult = Default::default();
        let now = Instant::now();

        // grab mutex to prevent new timelines from being created here.
        let gc_cs = self.gc_cs.lock().unwrap();

        let timelines = self.timelines.lock().unwrap();

        // Scan all timelines. For each timeline, remember the timeline ID and
        // the branch point where it was created.
        let mut all_branchpoints: BTreeSet<(TimelineId, Lsn)> = BTreeSet::new();
        let timeline_ids = {
            if let Some(target_timeline_id) = target_timeline_id.as_ref() {
                if timelines.get(target_timeline_id).is_none() {
                    bail!("gc target timeline does not exist")
                }
            };

            timelines
                .iter()
                .filter(|(_, timeline)| timeline.is_active())
                .map(|(timeline_id, timeline_entry)| {
                    // This is unresolved question for now, how to do gc in presence of remote timelines
                    // especially when this is combined with branching.
                    // Somewhat related: https://github.com/neondatabase/neon/issues/999
                    if let Some(ancestor_timeline_id) = &timeline_entry.get_ancestor_timeline_id() {
                        // If target_timeline is specified, we only need to know branchpoints of its children
                        if let Some(timeline_id) = target_timeline_id {
                            if ancestor_timeline_id == &timeline_id {
                                all_branchpoints.insert((
                                    *ancestor_timeline_id,
                                    timeline_entry.get_ancestor_lsn(),
                                ));
                            }
                        }
                        // Collect branchpoints for all timelines
                        else {
                            all_branchpoints
                                .insert((*ancestor_timeline_id, timeline_entry.get_ancestor_lsn()));
                        }
                    }

                    *timeline_id
                })
                .collect::<Vec<_>>()
        };
        drop(timelines);

        // Ok, we now know all the branch points.
        // Update the GC information for each timeline.
        let mut gc_timelines = Vec::with_capacity(timeline_ids.len());
        for timeline_id in timeline_ids {
            // Timeline is known to be local and loaded.
            let timeline = self
                .get_timeline(timeline_id, false)
                .with_context(|| format!("Timeline {timeline_id} was not found"))?;

            // If target_timeline is specified, ignore all other timelines
            if let Some(target_timeline_id) = target_timeline_id {
                if timeline_id != target_timeline_id {
                    continue;
                }
            }

            if let Some(cutoff) = timeline.get_last_record_lsn().checked_sub(horizon) {
                let branchpoints: Vec<Lsn> = all_branchpoints
                    .range((
                        Included((timeline_id, Lsn(0))),
                        Included((timeline_id, Lsn(u64::MAX))),
                    ))
                    .map(|&x| x.1)
                    .collect();
                timeline.update_gc_info(branchpoints, cutoff, pitr)?;

                gc_timelines.push(timeline);
            }
        }
        drop(gc_cs);

        // Perform GC for each timeline.
        //
        // Note that we don't hold the GC lock here because we don't want
        // to delay the branch creation task, which requires the GC lock.
        // A timeline GC iteration can be slow because it may need to wait for
        // compaction (both require `layer_removal_cs` lock),
        // but the GC iteration can run concurrently with branch creation.
        //
        // See comments in [`Tenant::branch_timeline`] for more information
        // about why branch creation task can run concurrently with timeline's GC iteration.
        for timeline in gc_timelines {
            if task_mgr::is_shutdown_requested() {
                // We were requested to shut down. Stop and return with the progress we
                // made.
                break;
            }

            // If requested, force flush all in-memory layers to disk first,
            // so that they too can be garbage collected. That's
            // used in tests, so we want as deterministic results as possible.
            if checkpoint_before_gc {
                timeline.checkpoint(CheckpointConfig::Forced)?;
                info!(
                    "timeline {} checkpoint_before_gc done",
                    timeline.timeline_id
                );
            }

            let result = timeline.gc()?;
            totals += result;
        }

        totals.elapsed = now.elapsed();
        Ok(totals)
    }

    /// Branch an existing timeline
    fn branch_timeline(
        &self,
        src: TimelineId,
        dst: TimelineId,
        start_lsn: Option<Lsn>,
    ) -> anyhow::Result<Arc<Timeline>> {
        // We need to hold this lock to prevent GC from starting at the same time. GC scans the directory to learn
        // about timelines, so otherwise a race condition is possible, where we create new timeline and GC
        // concurrently removes data that is needed by the new timeline.
        let _gc_cs = self.gc_cs.lock().unwrap();
        let timelines = self.timelines.lock().unwrap();
        let timeline_uninit_mark = self.create_timeline_uninit_mark(dst, &timelines)?;
        drop(timelines);

        // In order for the branch creation task to not wait for GC/compaction,
        // we need to make sure that the starting LSN of the child branch is not out of scope midway by
        //
        // 1. holding the GC lock to prevent overwritting timeline's GC data
        // 2. checking both the latest GC cutoff LSN and latest GC info of the source timeline
        //
        // Step 2 is to avoid initializing the new branch using data removed by past GC iterations
        // or in-queue GC iterations.

        let src_timeline = self.get_timeline(src, false).with_context(|| {
            format!(
                "No ancestor {} found for timeline {}/{}",
                src, self.tenant_id, dst
            )
        })?;

        let latest_gc_cutoff_lsn = src_timeline.get_latest_gc_cutoff_lsn();

        // If no start LSN is specified, we branch the new timeline from the source timeline's last record LSN
        let start_lsn = start_lsn.unwrap_or_else(|| {
            let lsn = src_timeline.get_last_record_lsn();
            info!("branching timeline {dst} from timeline {src} at last record LSN: {lsn}");
            lsn
        });

        // Check if the starting LSN is out of scope because it is less than
        // 1. the latest GC cutoff LSN or
        // 2. the planned GC cutoff LSN, which is from an in-queue GC iteration.
        src_timeline
            .check_lsn_is_in_scope(start_lsn, &latest_gc_cutoff_lsn)
            .context(format!(
                "invalid branch start lsn: less than latest GC cutoff {}",
                *latest_gc_cutoff_lsn,
            ))?;
        {
            let gc_info = src_timeline.gc_info.read().unwrap();
            let cutoff = min(gc_info.pitr_cutoff, gc_info.horizon_cutoff);
            if start_lsn < cutoff {
                bail!(format!(
                    "invalid branch start lsn: less than planned GC cutoff {cutoff}"
                ));
            }
        }

        // Determine prev-LSN for the new timeline. We can only determine it if
        // the timeline was branched at the current end of the source timeline.
        let RecordLsn {
            last: src_last,
            prev: src_prev,
        } = src_timeline.get_last_record_rlsn();
        let dst_prev = if src_last == start_lsn {
            Some(src_prev)
        } else {
            None
        };

        // Create the metadata file, noting the ancestor of the new timeline.
        // There is initially no data in it, but all the read-calls know to look
        // into the ancestor.
        let metadata = TimelineMetadata::new(
            start_lsn,
            dst_prev,
            Some(src),
            start_lsn,
            *src_timeline.latest_gc_cutoff_lsn.read(), // FIXME: should we hold onto this guard longer?
            src_timeline.initdb_lsn,
            src_timeline.pg_version,
        );
        let mut timelines = self.timelines.lock().unwrap();
        let new_timeline = self
            .prepare_timeline(
                dst,
                metadata,
                timeline_uninit_mark,
                false,
                Some(src_timeline),
            )?
            .initialize_with_lock(&mut timelines, true)?;
        drop(timelines);
        info!("branched timeline {dst} from {src} at {start_lsn}");

        Ok(new_timeline)
    }

    /// - run initdb to init temporary instance and get bootstrap data
    /// - after initialization complete, remove the temp dir.
    fn bootstrap_timeline(
        &self,
        timeline_id: TimelineId,
        pg_version: u32,
    ) -> anyhow::Result<Arc<Timeline>> {
        let timelines = self.timelines.lock().unwrap();
        let timeline_uninit_mark = self.create_timeline_uninit_mark(timeline_id, &timelines)?;
        drop(timelines);
        // create a `tenant/{tenant_id}/timelines/basebackup-{timeline_id}.{TEMP_FILE_SUFFIX}/`
        // temporary directory for basebackup files for the given timeline.
        let initdb_path = path_with_suffix_extension(
            self.conf
                .timelines_path(&self.tenant_id)
                .join(format!("basebackup-{timeline_id}")),
            TEMP_FILE_SUFFIX,
        );

        // an uninit mark was placed before, nothing else can access this timeline files
        // current initdb was not run yet, so remove whatever was left from the previous runs
        if initdb_path.exists() {
            fs::remove_dir_all(&initdb_path).with_context(|| {
                format!(
                    "Failed to remove already existing initdb directory: {}",
                    initdb_path.display()
                )
            })?;
        }
        // Init temporarily repo to get bootstrap data, this creates a directory in the `initdb_path` path
        run_initdb(self.conf, &initdb_path, pg_version)?;
        // this new directory is very temporary, set to remove it immediately after bootstrap, we don't need it
        scopeguard::defer! {
            if let Err(e) = fs::remove_dir_all(&initdb_path) {
                // this is unlikely, but we will remove the directory on pageserver restart or another bootstrap call
                error!("Failed to remove temporary initdb directory '{}': {}", initdb_path.display(), e);
            }
        }
        let pgdata_path = &initdb_path;
        let pgdata_lsn = import_datadir::get_lsn_from_controlfile(pgdata_path)?.align();

        // Import the contents of the data directory at the initial checkpoint
        // LSN, and any WAL after that.
        // Initdb lsn will be equal to last_record_lsn which will be set after import.
        // Because we know it upfront avoid having an option or dummy zero value by passing it to the metadata.
        let new_metadata = TimelineMetadata::new(
            Lsn(0),
            None,
            None,
            Lsn(0),
            pgdata_lsn,
            pgdata_lsn,
            pg_version,
        );
        let raw_timeline =
            self.prepare_timeline(timeline_id, new_metadata, timeline_uninit_mark, true, None)?;

        let tenant_id = raw_timeline.owning_tenant.tenant_id;
        let unfinished_timeline = raw_timeline.raw_timeline()?;
        import_datadir::import_timeline_from_postgres_datadir(
            unfinished_timeline,
            pgdata_path,
            pgdata_lsn,
        )
        .with_context(|| {
            format!("Failed to import pgdatadir for timeline {tenant_id}/{timeline_id}")
        })?;

        fail::fail_point!("before-checkpoint-new-timeline", |_| {
            anyhow::bail!("failpoint before-checkpoint-new-timeline");
        });
        unfinished_timeline
            .checkpoint(CheckpointConfig::Forced)
            .with_context(|| format!("Failed to checkpoint after pgdatadir import for timeline {tenant_id}/{timeline_id}"))?;

        let mut timelines = self.timelines.lock().unwrap();
        let timeline = raw_timeline.initialize_with_lock(&mut timelines, false)?;
        drop(timelines);

        info!(
            "created root timeline {} timeline.lsn {}",
            timeline_id,
            timeline.get_last_record_lsn()
        );

        Ok(timeline)
    }

    /// Creates intermediate timeline structure and its files, without loading it into memory.
    /// It's up to the caller to import the necesary data and import the timeline into memory.
    fn prepare_timeline(
        &self,
        new_timeline_id: TimelineId,
        new_metadata: TimelineMetadata,
        uninit_mark: TimelineUninitMark,
        init_layers: bool,
        ancestor: Option<Arc<Timeline>>,
    ) -> anyhow::Result<UninitializedTimeline> {
        let tenant_id = self.tenant_id;

        match self.create_timeline_files(
            &uninit_mark.timeline_path,
            new_timeline_id,
            new_metadata,
            ancestor,
        ) {
            Ok(new_timeline) => {
                if init_layers {
                    new_timeline.layers.write().unwrap().next_open_layer_at =
                        Some(new_timeline.initdb_lsn);
                }
                debug!(
                    "Successfully created initial files for timeline {tenant_id}/{new_timeline_id}"
                );
                Ok(UninitializedTimeline {
                    owning_tenant: self,
                    timeline_id: new_timeline_id,
                    raw_timeline: Some((new_timeline, uninit_mark)),
                })
            }
            Err(e) => {
                error!("Failed to create initial files for timeline {tenant_id}/{new_timeline_id}, cleaning up: {e:?}");
                cleanup_timeline_directory(uninit_mark);
                Err(e)
            }
        }
    }

    fn create_timeline_files(
        &self,
        timeline_path: &Path,
        new_timeline_id: TimelineId,
        new_metadata: TimelineMetadata,
        ancestor: Option<Arc<Timeline>>,
    ) -> anyhow::Result<Timeline> {
        let timeline_data = self
            .create_timeline_data(new_timeline_id, new_metadata.clone(), ancestor)
            .context("Failed to create timeline data structure")?;
        crashsafe::create_dir_all(timeline_path).context("Failed to create timeline directory")?;

        fail::fail_point!("after-timeline-uninit-mark-creation", |_| {
            anyhow::bail!("failpoint after-timeline-uninit-mark-creation");
        });

        save_metadata(
            self.conf,
            new_timeline_id,
            self.tenant_id,
            &new_metadata,
            true,
        )
        .context("Failed to create timeline metadata")?;

        Ok(timeline_data)
    }

    /// Attempts to create an uninit mark file for the timeline initialization.
    /// Bails, if the timeline is already loaded into the memory (i.e. initialized before), or the uninit mark file already exists.
    ///
    /// This way, we need to hold the timelines lock only for small amount of time during the mark check/creation per timeline init.
    fn create_timeline_uninit_mark(
        &self,
        timeline_id: TimelineId,
        timelines: &MutexGuard<HashMap<TimelineId, Arc<Timeline>>>,
    ) -> anyhow::Result<TimelineUninitMark> {
        let tenant_id = self.tenant_id;

        anyhow::ensure!(
            timelines.get(&timeline_id).is_none(),
            "Timeline {tenant_id}/{timeline_id} already exists in pageserver's memory"
        );
        let timeline_path = self.conf.timeline_path(&timeline_id, &tenant_id);
        anyhow::ensure!(
            !timeline_path.exists(),
            "Timeline {} already exists, cannot create its uninit mark file",
            timeline_path.display()
        );

        let uninit_mark_path = self
            .conf
            .timeline_uninit_mark_file_path(tenant_id, timeline_id);
        fs::File::create(&uninit_mark_path)
            .context("Failed to create uninit mark file")
            .and_then(|_| {
                crashsafe::fsync_file_and_parent(&uninit_mark_path)
                    .context("Failed to fsync uninit mark file")
            })
            .with_context(|| {
                format!("Failed to crate uninit mark for timeline {tenant_id}/{timeline_id}")
            })?;

        let uninit_mark = TimelineUninitMark::new(uninit_mark_path, timeline_path);

        Ok(uninit_mark)
    }

    pub(super) fn init_attach_timelines(
        &self,
        timelines: HashMap<TimelineId, TimelineMetadata>,
    ) -> anyhow::Result<()> {
        let sorted_timelines = if timelines.len() == 1 {
            timelines.into_iter().collect()
        } else if !timelines.is_empty() {
            tree_sort_timelines(timelines)?
        } else {
            warn!("No timelines to attach received");
            return Ok(());
        };

        let tenant_id = self.tenant_id;
        let mut timelines_accessor = self.timelines.lock().unwrap();
        for (timeline_id, metadata) in sorted_timelines {
            info!(
                "Attaching timeline {}/{} pg_version {}",
                tenant_id,
                timeline_id,
                metadata.pg_version()
            );

            if timelines_accessor.contains_key(&timeline_id) {
                warn!("Timeline {tenant_id}/{timeline_id} already exists in the tenant map, skipping its initialization");
                continue;
            }

            let ancestor = metadata
                .ancestor_timeline()
                .and_then(|ancestor_timeline_id| timelines_accessor.get(&ancestor_timeline_id))
                .cloned();
            let dummy_timeline = self
                .create_timeline_data(timeline_id, metadata.clone(), ancestor.clone())
                .with_context(|| {
                    format!("Failed to crate dummy timeline data for {tenant_id}/{timeline_id}")
                })?;
            let timeline = UninitializedTimeline {
                owning_tenant: self,
                timeline_id,
                raw_timeline: Some((dummy_timeline, TimelineUninitMark::dummy())),
            };
            match timeline.initialize_with_lock(&mut timelines_accessor, true) {
                Ok(initialized_timeline) => {
                    timelines_accessor.insert(timeline_id, initialized_timeline);
                }
                Err(e) => {
                    error!("Failed to initialize timeline {tenant_id}/{timeline_id}: {e:?}");
                    let broken_timeline = self
                        .create_timeline_data(timeline_id, metadata, ancestor)
                        .with_context(|| {
                            format!("Failed to crate broken timeline data for {tenant_id}/{timeline_id}")
                        })?;
                    broken_timeline.set_state(TimelineState::Broken);
                    timelines_accessor.insert(timeline_id, Arc::new(broken_timeline));
                }
            }
        }

        Ok(())
    }
}

/// Create the cluster temporarily in 'initdbpath' directory inside the repository
/// to get bootstrap data for timeline initialization.
fn run_initdb(
    conf: &'static PageServerConf,
    initdb_target_dir: &Path,
    pg_version: u32,
) -> anyhow::Result<()> {
    let initdb_bin_path = conf.pg_bin_dir(pg_version)?.join("initdb");
    let initdb_lib_dir = conf.pg_lib_dir(pg_version)?;
    info!(
        "running {} in {}, libdir: {}",
        initdb_bin_path.display(),
        initdb_target_dir.display(),
        initdb_lib_dir.display(),
    );

    let initdb_output = Command::new(&initdb_bin_path)
        .args(&["-D", &initdb_target_dir.to_string_lossy()])
        .args(&["-U", &conf.superuser])
        .args(&["-E", "utf8"])
        .arg("--no-instructions")
        // This is only used for a temporary installation that is deleted shortly after,
        // so no need to fsync it
        .arg("--no-sync")
        .env_clear()
        .env("LD_LIBRARY_PATH", &initdb_lib_dir)
        .env("DYLD_LIBRARY_PATH", &initdb_lib_dir)
        .stdout(Stdio::null())
        .output()
        .with_context(|| {
            format!(
                "failed to execute {} at target dir {}",
                initdb_bin_path.display(),
                initdb_target_dir.display()
            )
        })?;
    if !initdb_output.status.success() {
        bail!(
            "initdb failed: '{}'",
            String::from_utf8_lossy(&initdb_output.stderr)
        );
    }

    Ok(())
}

impl Drop for Tenant {
    fn drop(&mut self) {
        remove_tenant_metrics(&self.tenant_id);
    }
}
/// Dump contents of a layer file to stdout.
pub fn dump_layerfile_from_path(path: &Path, verbose: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::FileExt;

    // All layer files start with a two-byte "magic" value, to identify the kind of
    // file.
    let file = File::open(path)?;
    let mut header_buf = [0u8; 2];
    file.read_exact_at(&mut header_buf, 0)?;

    match u16::from_be_bytes(header_buf) {
        crate::IMAGE_FILE_MAGIC => {
            image_layer::ImageLayer::new_for_path(path, file)?.dump(verbose)?
        }
        crate::DELTA_FILE_MAGIC => {
            delta_layer::DeltaLayer::new_for_path(path, file)?.dump(verbose)?
        }
        magic => bail!("unrecognized magic identifier: {:?}", magic),
    }

    Ok(())
}

fn ignore_absent_files<F>(fs_operation: F) -> io::Result<()>
where
    F: Fn() -> io::Result<()>,
{
    fs_operation().or_else(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(e)
        }
    })
}

#[cfg(test)]
pub mod harness {
    use bytes::{Bytes, BytesMut};
    use once_cell::sync::Lazy;
    use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
    use std::{fs, path::PathBuf};
    use utils::lsn::Lsn;

    use crate::storage_sync::index::RemoteIndex;
    use crate::{
        config::PageServerConf,
        repository::Key,
        tenant::Tenant,
        walrecord::NeonWalRecord,
        walredo::{WalRedoError, WalRedoManager},
    };

    use super::*;
    use crate::tenant_config::{TenantConf, TenantConfOpt};
    use hex_literal::hex;
    use utils::id::{TenantId, TimelineId};

    pub const TIMELINE_ID: TimelineId =
        TimelineId::from_array(hex!("11223344556677881122334455667788"));
    pub const NEW_TIMELINE_ID: TimelineId =
        TimelineId::from_array(hex!("AA223344556677881122334455667788"));

    /// Convenience function to create a page image with given string as the only content
    #[allow(non_snake_case)]
    pub fn TEST_IMG(s: &str) -> Bytes {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(s.as_bytes());
        buf.resize(64, 0);

        buf.freeze()
    }

    static LOCK: Lazy<RwLock<()>> = Lazy::new(|| RwLock::new(()));

    impl From<TenantConf> for TenantConfOpt {
        fn from(tenant_conf: TenantConf) -> Self {
            Self {
                checkpoint_distance: Some(tenant_conf.checkpoint_distance),
                checkpoint_timeout: Some(tenant_conf.checkpoint_timeout),
                compaction_target_size: Some(tenant_conf.compaction_target_size),
                compaction_period: Some(tenant_conf.compaction_period),
                compaction_threshold: Some(tenant_conf.compaction_threshold),
                gc_horizon: Some(tenant_conf.gc_horizon),
                gc_period: Some(tenant_conf.gc_period),
                image_creation_threshold: Some(tenant_conf.image_creation_threshold),
                pitr_interval: Some(tenant_conf.pitr_interval),
                walreceiver_connect_timeout: Some(tenant_conf.walreceiver_connect_timeout),
                lagging_wal_timeout: Some(tenant_conf.lagging_wal_timeout),
                max_lsn_wal_lag: Some(tenant_conf.max_lsn_wal_lag),
            }
        }
    }

    pub struct TenantHarness<'a> {
        pub conf: &'static PageServerConf,
        pub tenant_conf: TenantConf,
        pub tenant_id: TenantId,

        pub lock_guard: (
            Option<RwLockReadGuard<'a, ()>>,
            Option<RwLockWriteGuard<'a, ()>>,
        ),
    }

    impl<'a> TenantHarness<'a> {
        pub fn create(test_name: &'static str) -> anyhow::Result<Self> {
            Self::create_internal(test_name, false)
        }
        pub fn create_exclusive(test_name: &'static str) -> anyhow::Result<Self> {
            Self::create_internal(test_name, true)
        }
        fn create_internal(test_name: &'static str, exclusive: bool) -> anyhow::Result<Self> {
            let lock_guard = if exclusive {
                (None, Some(LOCK.write().unwrap()))
            } else {
                (Some(LOCK.read().unwrap()), None)
            };

            let repo_dir = PageServerConf::test_repo_dir(test_name);
            let _ = fs::remove_dir_all(&repo_dir);
            fs::create_dir_all(&repo_dir)?;

            let conf = PageServerConf::dummy_conf(repo_dir);
            // Make a static copy of the config. This can never be free'd, but that's
            // OK in a test.
            let conf: &'static PageServerConf = Box::leak(Box::new(conf));

            let tenant_conf = TenantConf::dummy_conf();

            let tenant_id = TenantId::generate();
            fs::create_dir_all(conf.tenant_path(&tenant_id))?;
            fs::create_dir_all(conf.timelines_path(&tenant_id))?;

            Ok(Self {
                conf,
                tenant_conf,
                tenant_id,
                lock_guard,
            })
        }

        pub fn load(&self) -> Tenant {
            self.try_load().expect("failed to load test tenant")
        }

        pub fn try_load(&self) -> anyhow::Result<Tenant> {
            let walredo_mgr = Arc::new(TestRedoManager);

            let tenant = Tenant::new(
                self.conf,
                TenantConfOpt::from(self.tenant_conf),
                walredo_mgr,
                self.tenant_id,
                RemoteIndex::default(),
                false,
            );
            // populate tenant with locally available timelines
            let mut timelines_to_load = HashMap::new();
            for timeline_dir_entry in fs::read_dir(self.conf.timelines_path(&self.tenant_id))
                .expect("should be able to read timelines dir")
            {
                let timeline_dir_entry = timeline_dir_entry?;
                let timeline_id: TimelineId = timeline_dir_entry
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .parse()?;

                let timeline_metadata = load_metadata(self.conf, timeline_id, self.tenant_id)?;
                timelines_to_load.insert(timeline_id, timeline_metadata);
            }
            tenant.init_attach_timelines(timelines_to_load)?;
            tenant.set_state(TenantState::Active {
                background_jobs_running: false,
            });

            Ok(tenant)
        }

        pub fn timeline_path(&self, timeline_id: &TimelineId) -> PathBuf {
            self.conf.timeline_path(timeline_id, &self.tenant_id)
        }
    }

    fn load_metadata(
        conf: &'static PageServerConf,
        timeline_id: TimelineId,
        tenant_id: TenantId,
    ) -> anyhow::Result<TimelineMetadata> {
        let metadata_path = conf.metadata_path(timeline_id, tenant_id);
        let metadata_bytes = std::fs::read(&metadata_path).with_context(|| {
            format!(
                "Failed to read metadata bytes from path {}",
                metadata_path.display()
            )
        })?;
        TimelineMetadata::from_bytes(&metadata_bytes).with_context(|| {
            format!(
                "Failed to parse metadata bytes from path {}",
                metadata_path.display()
            )
        })
    }

    // Mock WAL redo manager that doesn't do much
    pub struct TestRedoManager;

    impl WalRedoManager for TestRedoManager {
        fn request_redo(
            &self,
            key: Key,
            lsn: Lsn,
            base_img: Option<Bytes>,
            records: Vec<(Lsn, NeonWalRecord)>,
            _pg_version: u32,
        ) -> Result<Bytes, WalRedoError> {
            let s = format!(
                "redo for {} to get to {}, with {} and {} records",
                key,
                lsn,
                if base_img.is_some() {
                    "base image"
                } else {
                    "no base image"
                },
                records.len()
            );
            println!("{s}");

            Ok(TEST_IMG(&s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::METADATA_FILE_NAME;
    use crate::keyspace::KeySpaceAccum;
    use crate::repository::{Key, Value};
    use crate::tenant::harness::*;
    use crate::DEFAULT_PG_VERSION;
    use bytes::BytesMut;
    use hex_literal::hex;
    use once_cell::sync::Lazy;
    use rand::{thread_rng, Rng};

    static TEST_KEY: Lazy<Key> =
        Lazy::new(|| Key::from_slice(&hex!("112222222233333333444444445500000001")));

    #[test]
    fn test_basic() -> anyhow::Result<()> {
        let tenant = TenantHarness::create("test_basic")?.load();
        let tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x10), &Value::Image(TEST_IMG("foo at 0x10")))?;
        writer.finish_write(Lsn(0x10));
        drop(writer);

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x20), &Value::Image(TEST_IMG("foo at 0x20")))?;
        writer.finish_write(Lsn(0x20));
        drop(writer);

        assert_eq!(tline.get(*TEST_KEY, Lsn(0x10))?, TEST_IMG("foo at 0x10"));
        assert_eq!(tline.get(*TEST_KEY, Lsn(0x1f))?, TEST_IMG("foo at 0x10"));
        assert_eq!(tline.get(*TEST_KEY, Lsn(0x20))?, TEST_IMG("foo at 0x20"));

        Ok(())
    }

    #[test]
    fn no_duplicate_timelines() -> anyhow::Result<()> {
        let tenant = TenantHarness::create("no_duplicate_timelines")?.load();
        let _ = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;

        match tenant.create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION) {
            Ok(_) => panic!("duplicate timeline creation should fail"),
            Err(e) => assert_eq!(
                e.to_string(),
                format!(
                    "Timeline {}/{} already exists in pageserver's memory",
                    tenant.tenant_id, TIMELINE_ID
                )
            ),
        }

        Ok(())
    }

    /// Convenience function to create a page image with given string as the only content
    pub fn test_value(s: &str) -> Value {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(s.as_bytes());
        Value::Image(buf.freeze())
    }

    ///
    /// Test branch creation
    ///
    #[test]
    fn test_branch() -> anyhow::Result<()> {
        let tenant = TenantHarness::create("test_branch")?.load();
        let tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;
        let writer = tline.writer();
        use std::str::from_utf8;

        #[allow(non_snake_case)]
        let TEST_KEY_A: Key = Key::from_hex("112222222233333333444444445500000001").unwrap();
        #[allow(non_snake_case)]
        let TEST_KEY_B: Key = Key::from_hex("112222222233333333444444445500000002").unwrap();

        // Insert a value on the timeline
        writer.put(TEST_KEY_A, Lsn(0x20), &test_value("foo at 0x20"))?;
        writer.put(TEST_KEY_B, Lsn(0x20), &test_value("foobar at 0x20"))?;
        writer.finish_write(Lsn(0x20));

        writer.put(TEST_KEY_A, Lsn(0x30), &test_value("foo at 0x30"))?;
        writer.finish_write(Lsn(0x30));
        writer.put(TEST_KEY_A, Lsn(0x40), &test_value("foo at 0x40"))?;
        writer.finish_write(Lsn(0x40));

        //assert_current_logical_size(&tline, Lsn(0x40));

        // Branch the history, modify relation differently on the new timeline
        tenant.branch_timeline(TIMELINE_ID, NEW_TIMELINE_ID, Some(Lsn(0x30)))?;
        let newtline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("Should have a local timeline");
        let new_writer = newtline.writer();
        new_writer.put(TEST_KEY_A, Lsn(0x40), &test_value("bar at 0x40"))?;
        new_writer.finish_write(Lsn(0x40));

        // Check page contents on both branches
        assert_eq!(
            from_utf8(&tline.get(TEST_KEY_A, Lsn(0x40))?)?,
            "foo at 0x40"
        );
        assert_eq!(
            from_utf8(&newtline.get(TEST_KEY_A, Lsn(0x40))?)?,
            "bar at 0x40"
        );
        assert_eq!(
            from_utf8(&newtline.get(TEST_KEY_B, Lsn(0x40))?)?,
            "foobar at 0x20"
        );

        //assert_current_logical_size(&tline, Lsn(0x40));

        Ok(())
    }

    fn make_some_layers(tline: &Timeline, start_lsn: Lsn) -> anyhow::Result<()> {
        let mut lsn = start_lsn;
        #[allow(non_snake_case)]
        {
            let writer = tline.writer();
            // Create a relation on the timeline
            writer.put(
                *TEST_KEY,
                lsn,
                &Value::Image(TEST_IMG(&format!("foo at {}", lsn))),
            )?;
            writer.finish_write(lsn);
            lsn += 0x10;
            writer.put(
                *TEST_KEY,
                lsn,
                &Value::Image(TEST_IMG(&format!("foo at {}", lsn))),
            )?;
            writer.finish_write(lsn);
            lsn += 0x10;
        }
        tline.checkpoint(CheckpointConfig::Forced)?;
        {
            let writer = tline.writer();
            writer.put(
                *TEST_KEY,
                lsn,
                &Value::Image(TEST_IMG(&format!("foo at {}", lsn))),
            )?;
            writer.finish_write(lsn);
            lsn += 0x10;
            writer.put(
                *TEST_KEY,
                lsn,
                &Value::Image(TEST_IMG(&format!("foo at {}", lsn))),
            )?;
            writer.finish_write(lsn);
        }
        tline.checkpoint(CheckpointConfig::Forced)
    }

    #[test]
    fn test_prohibit_branch_creation_on_garbage_collected_data() -> anyhow::Result<()> {
        let tenant =
            TenantHarness::create("test_prohibit_branch_creation_on_garbage_collected_data")?
                .load();
        let tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;
        make_some_layers(tline.as_ref(), Lsn(0x20))?;

        // this removes layers before lsn 40 (50 minus 10), so there are two remaining layers, image and delta for 31-50
        // FIXME: this doesn't actually remove any layer currently, given how the checkpointing
        // and compaction works. But it does set the 'cutoff' point so that the cross check
        // below should fail.
        tenant.gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO, false)?;

        // try to branch at lsn 25, should fail because we already garbage collected the data
        match tenant.branch_timeline(TIMELINE_ID, NEW_TIMELINE_ID, Some(Lsn(0x25))) {
            Ok(_) => panic!("branching should have failed"),
            Err(err) => {
                assert!(err.to_string().contains("invalid branch start lsn"));
                assert!(err
                    .source()
                    .unwrap()
                    .to_string()
                    .contains("we might've already garbage collected needed data"))
            }
        }

        Ok(())
    }

    #[test]
    fn test_prohibit_branch_creation_on_pre_initdb_lsn() -> anyhow::Result<()> {
        let tenant =
            TenantHarness::create("test_prohibit_branch_creation_on_pre_initdb_lsn")?.load();

        tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0x50), DEFAULT_PG_VERSION)?
            .initialize()?;
        // try to branch at lsn 0x25, should fail because initdb lsn is 0x50
        match tenant.branch_timeline(TIMELINE_ID, NEW_TIMELINE_ID, Some(Lsn(0x25))) {
            Ok(_) => panic!("branching should have failed"),
            Err(err) => {
                assert!(&err.to_string().contains("invalid branch start lsn"));
                assert!(&err
                    .source()
                    .unwrap()
                    .to_string()
                    .contains("is earlier than latest GC horizon"));
            }
        }

        Ok(())
    }

    /*
    // FIXME: This currently fails to error out. Calling GC doesn't currently
    // remove the old value, we'd need to work a little harder
    #[test]
    fn test_prohibit_get_for_garbage_collected_data() -> anyhow::Result<()> {
        let repo =
            RepoHarness::create("test_prohibit_get_for_garbage_collected_data")?
            .load();

        let tline = repo.create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?;
        make_some_layers(tline.as_ref(), Lsn(0x20))?;

        repo.gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO, false)?;
        let latest_gc_cutoff_lsn = tline.get_latest_gc_cutoff_lsn();
        assert!(*latest_gc_cutoff_lsn > Lsn(0x25));
        match tline.get(*TEST_KEY, Lsn(0x25)) {
            Ok(_) => panic!("request for page should have failed"),
            Err(err) => assert!(err.to_string().contains("not found at")),
        }
        Ok(())
    }
     */

    #[test]
    fn test_retain_data_in_parent_which_is_needed_for_child() -> anyhow::Result<()> {
        let tenant =
            TenantHarness::create("test_retain_data_in_parent_which_is_needed_for_child")?.load();
        let tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;
        make_some_layers(tline.as_ref(), Lsn(0x20))?;

        tenant.branch_timeline(TIMELINE_ID, NEW_TIMELINE_ID, Some(Lsn(0x40)))?;
        let newtline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("Should have a local timeline");
        // this removes layers before lsn 40 (50 minus 10), so there are two remaining layers, image and delta for 31-50
        tenant.gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO, false)?;
        assert!(newtline.get(*TEST_KEY, Lsn(0x25)).is_ok());

        Ok(())
    }
    #[test]
    fn test_parent_keeps_data_forever_after_branching() -> anyhow::Result<()> {
        let tenant =
            TenantHarness::create("test_parent_keeps_data_forever_after_branching")?.load();
        let tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;
        make_some_layers(tline.as_ref(), Lsn(0x20))?;

        tenant.branch_timeline(TIMELINE_ID, NEW_TIMELINE_ID, Some(Lsn(0x40)))?;
        let newtline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("Should have a local timeline");

        make_some_layers(newtline.as_ref(), Lsn(0x60))?;

        // run gc on parent
        tenant.gc_iteration(Some(TIMELINE_ID), 0x10, Duration::ZERO, false)?;

        // Check that the data is still accessible on the branch.
        assert_eq!(
            newtline.get(*TEST_KEY, Lsn(0x50))?,
            TEST_IMG(&format!("foo at {}", Lsn(0x40)))
        );

        Ok(())
    }

    #[test]
    fn timeline_load() -> anyhow::Result<()> {
        const TEST_NAME: &str = "timeline_load";
        let harness = TenantHarness::create(TEST_NAME)?;
        {
            let tenant = harness.load();
            let tline = tenant
                .create_empty_timeline(TIMELINE_ID, Lsn(0x8000), DEFAULT_PG_VERSION)?
                .initialize()?;
            make_some_layers(tline.as_ref(), Lsn(0x8000))?;
            tline.checkpoint(CheckpointConfig::Forced)?;
        }

        let tenant = harness.load();
        tenant
            .get_timeline(TIMELINE_ID, true)
            .expect("cannot load timeline");

        Ok(())
    }

    #[test]
    fn timeline_load_with_ancestor() -> anyhow::Result<()> {
        const TEST_NAME: &str = "timeline_load_with_ancestor";
        let harness = TenantHarness::create(TEST_NAME)?;
        // create two timelines
        {
            let tenant = harness.load();
            let tline = tenant
                .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
                .initialize()?;

            make_some_layers(tline.as_ref(), Lsn(0x20))?;
            tline.checkpoint(CheckpointConfig::Forced)?;

            tenant.branch_timeline(TIMELINE_ID, NEW_TIMELINE_ID, Some(Lsn(0x40)))?;

            let newtline = tenant
                .get_timeline(NEW_TIMELINE_ID, true)
                .expect("Should have a local timeline");

            make_some_layers(newtline.as_ref(), Lsn(0x60))?;
            tline.checkpoint(CheckpointConfig::Forced)?;
        }

        // check that both of them are initially unloaded
        let tenant = harness.load();

        // check that both, child and ancestor are loaded
        let _child_tline = tenant
            .get_timeline(NEW_TIMELINE_ID, true)
            .expect("cannot get child timeline loaded");

        let _ancestor_tline = tenant
            .get_timeline(TIMELINE_ID, true)
            .expect("cannot get ancestor timeline loaded");

        Ok(())
    }

    #[test]
    fn corrupt_metadata() -> anyhow::Result<()> {
        const TEST_NAME: &str = "corrupt_metadata";
        let harness = TenantHarness::create(TEST_NAME)?;
        let tenant = harness.load();

        tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;
        drop(tenant);

        let metadata_path = harness.timeline_path(&TIMELINE_ID).join(METADATA_FILE_NAME);

        assert!(metadata_path.is_file());

        let mut metadata_bytes = std::fs::read(&metadata_path)?;
        assert_eq!(metadata_bytes.len(), 512);
        metadata_bytes[8] ^= 1;
        std::fs::write(metadata_path, metadata_bytes)?;

        let err = harness.try_load().err().expect("should fail");
        assert!(err
            .to_string()
            .starts_with("Failed to parse metadata bytes from path"));

        let mut found_error_message = false;
        let mut err_source = err.source();
        while let Some(source) = err_source {
            if source.to_string() == "metadata checksum mismatch" {
                found_error_message = true;
                break;
            }
            err_source = source.source();
        }
        assert!(
            found_error_message,
            "didn't find the corrupted metadata error"
        );

        Ok(())
    }

    #[test]
    fn test_images() -> anyhow::Result<()> {
        let tenant = TenantHarness::create("test_images")?.load();
        let tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x10), &Value::Image(TEST_IMG("foo at 0x10")))?;
        writer.finish_write(Lsn(0x10));
        drop(writer);

        tline.checkpoint(CheckpointConfig::Forced)?;
        tline.compact()?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x20), &Value::Image(TEST_IMG("foo at 0x20")))?;
        writer.finish_write(Lsn(0x20));
        drop(writer);

        tline.checkpoint(CheckpointConfig::Forced)?;
        tline.compact()?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x30), &Value::Image(TEST_IMG("foo at 0x30")))?;
        writer.finish_write(Lsn(0x30));
        drop(writer);

        tline.checkpoint(CheckpointConfig::Forced)?;
        tline.compact()?;

        let writer = tline.writer();
        writer.put(*TEST_KEY, Lsn(0x40), &Value::Image(TEST_IMG("foo at 0x40")))?;
        writer.finish_write(Lsn(0x40));
        drop(writer);

        tline.checkpoint(CheckpointConfig::Forced)?;
        tline.compact()?;

        assert_eq!(tline.get(*TEST_KEY, Lsn(0x10))?, TEST_IMG("foo at 0x10"));
        assert_eq!(tline.get(*TEST_KEY, Lsn(0x1f))?, TEST_IMG("foo at 0x10"));
        assert_eq!(tline.get(*TEST_KEY, Lsn(0x20))?, TEST_IMG("foo at 0x20"));
        assert_eq!(tline.get(*TEST_KEY, Lsn(0x30))?, TEST_IMG("foo at 0x30"));
        assert_eq!(tline.get(*TEST_KEY, Lsn(0x40))?, TEST_IMG("foo at 0x40"));

        Ok(())
    }

    //
    // Insert 1000 key-value pairs with increasing keys, checkpoint,
    // repeat 50 times.
    //
    #[test]
    fn test_bulk_insert() -> anyhow::Result<()> {
        let tenant = TenantHarness::create("test_bulk_insert")?.load();
        let tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;

        let mut lsn = Lsn(0x10);

        let mut keyspace = KeySpaceAccum::new();

        let mut test_key = Key::from_hex("012222222233333333444444445500000000").unwrap();
        let mut blknum = 0;
        for _ in 0..50 {
            for _ in 0..10000 {
                test_key.field6 = blknum;
                let writer = tline.writer();
                writer.put(
                    test_key,
                    lsn,
                    &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
                )?;
                writer.finish_write(lsn);
                drop(writer);

                keyspace.add_key(test_key);

                lsn = Lsn(lsn.0 + 0x10);
                blknum += 1;
            }

            let cutoff = tline.get_last_record_lsn();

            tline.update_gc_info(Vec::new(), cutoff, Duration::ZERO)?;
            tline.checkpoint(CheckpointConfig::Forced)?;
            tline.compact()?;
            tline.gc()?;
        }

        Ok(())
    }

    #[test]
    fn test_random_updates() -> anyhow::Result<()> {
        let tenant = TenantHarness::create("test_random_updates")?.load();
        let tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;

        const NUM_KEYS: usize = 1000;

        let mut test_key = Key::from_hex("012222222233333333444444445500000000").unwrap();

        let mut keyspace = KeySpaceAccum::new();

        // Track when each page was last modified. Used to assert that
        // a read sees the latest page version.
        let mut updated = [Lsn(0); NUM_KEYS];

        let mut lsn = Lsn(0);
        #[allow(clippy::needless_range_loop)]
        for blknum in 0..NUM_KEYS {
            lsn = Lsn(lsn.0 + 0x10);
            test_key.field6 = blknum as u32;
            let writer = tline.writer();
            writer.put(
                test_key,
                lsn,
                &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
            )?;
            writer.finish_write(lsn);
            updated[blknum] = lsn;
            drop(writer);

            keyspace.add_key(test_key);
        }

        for _ in 0..50 {
            for _ in 0..NUM_KEYS {
                lsn = Lsn(lsn.0 + 0x10);
                let blknum = thread_rng().gen_range(0..NUM_KEYS);
                test_key.field6 = blknum as u32;
                let writer = tline.writer();
                writer.put(
                    test_key,
                    lsn,
                    &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
                )?;
                writer.finish_write(lsn);
                drop(writer);
                updated[blknum] = lsn;
            }

            // Read all the blocks
            for (blknum, last_lsn) in updated.iter().enumerate() {
                test_key.field6 = blknum as u32;
                assert_eq!(
                    tline.get(test_key, lsn)?,
                    TEST_IMG(&format!("{} at {}", blknum, last_lsn))
                );
            }

            // Perform a cycle of checkpoint, compaction, and GC
            println!("checkpointing {}", lsn);
            let cutoff = tline.get_last_record_lsn();
            tline.update_gc_info(Vec::new(), cutoff, Duration::ZERO)?;
            tline.checkpoint(CheckpointConfig::Forced)?;
            tline.compact()?;
            tline.gc()?;
        }

        Ok(())
    }

    #[test]
    fn test_traverse_branches() -> anyhow::Result<()> {
        let tenant = TenantHarness::create("test_traverse_branches")?.load();
        let mut tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;

        const NUM_KEYS: usize = 1000;

        let mut test_key = Key::from_hex("012222222233333333444444445500000000").unwrap();

        let mut keyspace = KeySpaceAccum::new();

        // Track when each page was last modified. Used to assert that
        // a read sees the latest page version.
        let mut updated = [Lsn(0); NUM_KEYS];

        let mut lsn = Lsn(0);
        #[allow(clippy::needless_range_loop)]
        for blknum in 0..NUM_KEYS {
            lsn = Lsn(lsn.0 + 0x10);
            test_key.field6 = blknum as u32;
            let writer = tline.writer();
            writer.put(
                test_key,
                lsn,
                &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
            )?;
            writer.finish_write(lsn);
            updated[blknum] = lsn;
            drop(writer);

            keyspace.add_key(test_key);
        }

        let mut tline_id = TIMELINE_ID;
        for _ in 0..50 {
            let new_tline_id = TimelineId::generate();
            tenant.branch_timeline(tline_id, new_tline_id, Some(lsn))?;
            tline = tenant
                .get_timeline(new_tline_id, true)
                .expect("Should have the branched timeline");
            tline_id = new_tline_id;

            for _ in 0..NUM_KEYS {
                lsn = Lsn(lsn.0 + 0x10);
                let blknum = thread_rng().gen_range(0..NUM_KEYS);
                test_key.field6 = blknum as u32;
                let writer = tline.writer();
                writer.put(
                    test_key,
                    lsn,
                    &Value::Image(TEST_IMG(&format!("{} at {}", blknum, lsn))),
                )?;
                println!("updating {} at {}", blknum, lsn);
                writer.finish_write(lsn);
                drop(writer);
                updated[blknum] = lsn;
            }

            // Read all the blocks
            for (blknum, last_lsn) in updated.iter().enumerate() {
                test_key.field6 = blknum as u32;
                assert_eq!(
                    tline.get(test_key, lsn)?,
                    TEST_IMG(&format!("{} at {}", blknum, last_lsn))
                );
            }

            // Perform a cycle of checkpoint, compaction, and GC
            println!("checkpointing {}", lsn);
            let cutoff = tline.get_last_record_lsn();
            tline.update_gc_info(Vec::new(), cutoff, Duration::ZERO)?;
            tline.checkpoint(CheckpointConfig::Forced)?;
            tline.compact()?;
            tline.gc()?;
        }

        Ok(())
    }

    #[test]
    fn test_traverse_ancestors() -> anyhow::Result<()> {
        let tenant = TenantHarness::create("test_traverse_ancestors")?.load();
        let mut tline = tenant
            .create_empty_timeline(TIMELINE_ID, Lsn(0), DEFAULT_PG_VERSION)?
            .initialize()?;

        const NUM_KEYS: usize = 100;
        const NUM_TLINES: usize = 50;

        let mut test_key = Key::from_hex("012222222233333333444444445500000000").unwrap();
        // Track page mutation lsns across different timelines.
        let mut updated = [[Lsn(0); NUM_KEYS]; NUM_TLINES];

        let mut lsn = Lsn(0);
        let mut tline_id = TIMELINE_ID;

        #[allow(clippy::needless_range_loop)]
        for idx in 0..NUM_TLINES {
            let new_tline_id = TimelineId::generate();
            tenant.branch_timeline(tline_id, new_tline_id, Some(lsn))?;
            tline = tenant
                .get_timeline(new_tline_id, true)
                .expect("Should have the branched timeline");
            tline_id = new_tline_id;

            for _ in 0..NUM_KEYS {
                lsn = Lsn(lsn.0 + 0x10);
                let blknum = thread_rng().gen_range(0..NUM_KEYS);
                test_key.field6 = blknum as u32;
                let writer = tline.writer();
                writer.put(
                    test_key,
                    lsn,
                    &Value::Image(TEST_IMG(&format!("{} {} at {}", idx, blknum, lsn))),
                )?;
                println!("updating [{}][{}] at {}", idx, blknum, lsn);
                writer.finish_write(lsn);
                drop(writer);
                updated[idx][blknum] = lsn;
            }
        }

        // Read pages from leaf timeline across all ancestors.
        for (idx, lsns) in updated.iter().enumerate() {
            for (blknum, lsn) in lsns.iter().enumerate() {
                // Skip empty mutations.
                if lsn.0 == 0 {
                    continue;
                }
                println!("checking [{idx}][{blknum}] at {lsn}");
                test_key.field6 = blknum as u32;
                assert_eq!(
                    tline.get(test_key, *lsn)?,
                    TEST_IMG(&format!("{idx} {blknum} at {lsn}"))
                );
            }
        }
        Ok(())
    }
}
