#![allow(unused_doc_comments)]

use collab_integrate::collab_builder::AppFlowyCollabBuilder;
use collab_integrate::instant_indexed_data_provider::InstantIndexedDataWriter;
use collab_plugins::CollabKVDB;
use flowy_ai::ai_manager::AIManager;
use flowy_database2::DatabaseManager;
use flowy_document::manager::DocumentManager;
use flowy_error::{FlowyError, FlowyResult};
use flowy_folder::manager::FolderManager;
use flowy_search::services::manager::SearchManager;
use flowy_server::af_cloud::define::LoggedUser;
use flowy_sqlite::kv::KVStorePreferences;
use flowy_storage::manager::StorageManager;
use flowy_user::services::authenticate_user::AuthenticateUser;
use flowy_user::services::entities::{UserConfig, UserPaths};
use flowy_user::user_manager::UserManager;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;
use sysinfo::System;
use tokio::sync::RwLock;
use tracing::{debug, error, event, info, instrument, warn};
use uuid::Uuid;
use flowy_backup::SnapshotManifest;

use lib_dispatch::prelude::*;
use lib_dispatch::runtime::AFPluginRuntime;
use lib_infra::priority_task::{TaskDispatcher, TaskRunner};
use lib_infra::util::{get_operating_system, OperatingSystem};
use lib_log::stream_log::StreamLogSender;
use module::make_plugins;

use crate::config::AppFlowyCoreConfig;
use crate::deps_resolve::file_storage_deps::FileStorageResolver;
use crate::deps_resolve::*;
use crate::full_indexed_data_provider::FullIndexedDataWriter;
use crate::log_filter::init_log;
use crate::server_layer::ServerProvider;
use app_life_cycle::AppLifeCycleImpl;
use deps_resolve::reminder_deps::CollabInteractImpl;
use flowy_sqlite::DBConnection;
use flowy_user_pub::entities::WorkspaceType;
use lib_infra::async_trait::async_trait;

pub(crate) mod app_life_cycle;
pub mod config;
mod deps_resolve;
pub mod backup_event;
mod folder_view_observer;
mod full_indexed_data_provider;
mod indexed_data_consumer;
mod indexing_data_runner;
mod log_filter;
pub mod module;
pub(crate) mod server_layer;

/// This name will be used as to identify the current [AppFlowyCore] instance.
/// Don't change this.
pub const DEFAULT_NAME: &str = "appflowy";

/// Result of a workspace backup operation, separating snapshot success from reopen success.
/// The backup must always attempt to reopen the workspace even if snapshot fails,
/// so we need to communicate both outcomes to the caller.
#[derive(Clone, Debug)]
pub struct WorkspaceBackupResult {
  pub manifest: SnapshotManifest,
  pub reopen_ok: bool,
  pub reopen_error: Option<String>,
}

// Re-export for FFI and public API
pub use self::WorkspaceBackupResult;

#[derive(Clone)]
pub struct AppFlowyCore {
  #[allow(dead_code)]
  pub config: AppFlowyCoreConfig,
  pub user_manager: Arc<UserManager>,
  pub document_manager: Arc<DocumentManager>,
  pub folder_manager: Arc<FolderManager>,
  pub database_manager: Arc<DatabaseManager>,
  pub event_dispatcher: Arc<AFPluginDispatcher>,
  pub server_provider: Arc<ServerProvider>,
  pub task_dispatcher: Arc<RwLock<TaskDispatcher>>,
  pub store_preference: Arc<KVStorePreferences>,
  pub search_manager: Arc<SearchManager>,
  pub ai_manager: Arc<AIManager>,
  pub storage_manager: Arc<StorageManager>,
  pub collab_builder: Arc<AppFlowyCollabBuilder>,
  pub full_indexed_data_writer: Arc<RwLock<Option<FullIndexedDataWriter>>>,
}

impl Drop for AppFlowyCore {
  fn drop(&mut self) {
    tracing::trace!("[Drop] drop appflowy core");
  }
}

impl AppFlowyCore {
  pub async fn new(
    config: AppFlowyCoreConfig,
    runtime: Arc<AFPluginRuntime>,
    stream_log_sender: Option<Arc<dyn StreamLogSender>>,
  ) -> Self {
    let platform = OperatingSystem::from(&config.platform);

    #[allow(clippy::if_same_then_else)]
    if cfg!(debug_assertions) {
      /// The profiling can be used to tracing the performance of the application.
      /// Check out the [Link](https://docs.appflowy.io/docs/documentation/software-contributions/architecture/backend/profiling#enable-profiling)
      ///  for more information.
      #[cfg(feature = "profiling")]
      console_subscriber::init();

      // Init the logger before anything else
      #[cfg(not(feature = "profiling"))]
      init_log(&config, &platform, stream_log_sender);
    } else {
      init_log(&config, &platform, stream_log_sender);
    }

    if sysinfo::IS_SUPPORTED_SYSTEM {
      info!(
        "💡{:?}, platform: {:?}",
        System::long_os_version(),
        platform
      );
    }

    Self::init(config, runtime).await
  }

  pub fn close_db(&self) {
    self.user_manager.close_db();
  }

  #[instrument(skip_all)]
  pub async fn run_workspace_backup(
    &self,
    staging_dir: &Path,
  ) -> FlowyResult<WorkspaceBackupResult> {
    use flowy_user_pub::sql::select_user_workspace_type;

    // Get current session to access user_id and workspace_id
    let session = self.user_manager.get_session()?;
    let user_id = session.user_id;
    let workspace_id = Uuid::parse_str(&session.workspace_id)?;

    // Query workspace type from database
    let mut conn = self.user_manager.db_connection(user_id)?;
    let workspace_type = select_user_workspace_type(&session.workspace_id, &mut conn)?;

    // Get user paths for snapshot location
    let data_root = self.config.storage_path.clone();
    let user_paths = UserPaths::new(data_root.clone());
    let user_dir = PathBuf::from(user_paths.user_data_dir(user_id));

    // FIX 1: Explicitly close all live collab objects for managers BEFORE closing collab_db
    // so their Weak references die and per-object flush plugins stop.
    debug!("Closing collab objects for backup quiesce");
    if let Err(err) = self.folder_manager.close_for_backup().await {
      error!("Failed to close folder manager for backup: {:?}", err);
    }
    if let Err(err) = self.database_manager.close_for_backup().await {
      error!("Failed to close database manager for backup: {:?}", err);
    }
    if let Err(err) = self.document_manager.close_for_backup().await {
      error!("Failed to close document manager for backup: {:?}", err);
    }

    // Capture snapshot result but always reopen/rebuild even on error (critical safety)
    let snapshot_result = async {
      // Step 2: Clear workspace awareness
      self.user_manager.clear_workspace_awareness(&workspace_id);
      debug!("Cleared workspace awareness");

      // Step 3: Close collab_db - this drops the sole Arc in UserDB::collab_db_map
      self.user_manager.close_collab_db(user_id)?;
      debug!("Closed collab_db");

      // Step 4: Take snapshot while collab_db is closed
      let snapshot_service = flowy_backup::SnapshotService::new(&data_root);
      let manifest = snapshot_service.snapshot(&user_dir, staging_dir)?;
      debug!("Snapshot completed");

      Ok::<SnapshotManifest, FlowyError>(manifest)
    }
    .await;

    // Step 5: Reopen and rebuild - MUST always run even if snapshot failed
    debug!("Reopening and rebuilding managers after backup");
    let mut reopen_ok = true;
    let mut reopen_error: Option<String> = None;

    let reopen_result = async {
      // Use default data source (LocalDisk) for reopening
      // This matches the logic in on_workspace_opened when data exists on disk
      use flowy_folder::manager::FolderInitDataSource;
      let data_source = FolderInitDataSource::LocalDisk {
        create_if_not_exist: false,
      };

      // Reinitialize all managers in the same sequence as on_workspace_opened
      self
        .folder_manager
        .initialize_after_open_workspace(user_id, data_source)
        .await?;
      debug!("Reinitialized folder manager");

      self
        .database_manager
        .initialize_after_open_workspace(user_id, workspace_type.is_local())
        .await?;
      debug!("Reinitialized database manager");

      self
        .document_manager
        .initialize_after_open_workspace(user_id)
        .await?;
      debug!("Reinitialized document manager");

      // FIX 4: Log storage manager init failures explicitly
      if let Err(err) = self
        .storage_manager
        .initialize_after_open_workspace(&workspace_id)
        .await
      {
        warn!("Storage manager initialization after backup failed: {:?}", err);
      }
      debug!("Reinitialized storage manager");

      // Re-run user awareness initialization using the public wrapper
      self
        .user_manager
        .reinit_user_awareness(
          user_id,
          &session.user_uuid,
          &workspace_id,
          &workspace_type,
        )
        .await?;
      debug!("Reinitialized user awareness");

      Ok::<(), FlowyError>(())
    }
    .await;

    // FIX 3: Surface reopen failures explicitly
    if let Err(err) = reopen_result {
      reopen_ok = false;
      reopen_error = Some(err.to_string());
      warn!("Failed to reopen managers after backup: app may be degraded: {:?}", err);
    }

    // Return partial success result - snapshot success is separate from reopen success
    match snapshot_result {
      Ok(manifest) => {
        Ok(WorkspaceBackupResult {
          manifest,
          reopen_ok,
          reopen_error,
        })
      },
      Err(err) => {
        // Even if snapshot failed, return the error with reopen status
        // This helps diagnose whether the app is still usable
        Err(err)
      }
    }
  }

  #[instrument(skip(config, runtime))]
  async fn init(config: AppFlowyCoreConfig, runtime: Arc<AFPluginRuntime>) -> Self {
    config.ensure_path();

    // Init the key value database
    let store_preference = Arc::new(KVStorePreferences::new(&config.storage_path).unwrap());
    info!("🔥{:?}", &config);

    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    flowy_ai::embeddings::context::EmbedContext::shared()
      .init_vector_db(PathBuf::from(&config.storage_path));

    let task_scheduler = TaskDispatcher::new(Duration::from_secs(10));
    let task_dispatcher = Arc::new(RwLock::new(task_scheduler));
    runtime.spawn(TaskRunner::run(task_dispatcher.clone()));

    let user_config = UserConfig::new(
      &config.name,
      &config.storage_path,
      &config.application_path,
      &config.device_id,
      config.app_version.clone(),
    );

    let authenticate_user = Arc::new(AuthenticateUser::new(
      user_config.clone(),
      store_preference.clone(),
    ));

    debug!("🔥runtime:{}", runtime);
    let instant_indexed_data_writer = if get_operating_system().is_desktop() {
      Some(Arc::new(InstantIndexedDataWriter::new()))
    } else {
      None
    };

    let server_provider = Arc::new(ServerProvider::new(
      config.clone(),
      Arc::downgrade(&store_preference),
      ServerUserImpl(Arc::downgrade(&authenticate_user)),
      instant_indexed_data_writer.as_ref().map(Arc::downgrade),
    ));

    event!(tracing::Level::DEBUG, "Init managers",);
    let (
      user_manager,
      folder_manager,
      server_provider,
      database_manager,
      document_manager,
      collab_builder,
      search_manager,
      ai_manager,
      storage_manager,
      instant_indexed_data_writer,
    ) = async {
      let storage_manager = FileStorageResolver::resolve(
        Arc::downgrade(&authenticate_user),
        server_provider.clone(),
        &user_config.storage_path,
      );

      /// The shared collab builder is used to build the [Collab] instance. The plugins will be loaded
      /// on demand based on the [CollabPluginConfig].
      let collab_builder = Arc::new(AppFlowyCollabBuilder::new(
        server_provider.clone(),
        WorkspaceCollabIntegrateImpl(Arc::downgrade(&authenticate_user)),
        instant_indexed_data_writer.as_ref().map(Arc::downgrade),
      ));

      collab_builder
        .set_snapshot_persistence(Arc::new(SnapshotDBImpl(Arc::downgrade(&authenticate_user))));

      let folder_manager = FolderDepsResolver::resolve(
        Arc::downgrade(&authenticate_user),
        collab_builder.clone(),
        Arc::downgrade(&server_provider),
        store_preference.clone(),
      )
      .await;

      let folder_query_service = FolderServiceImpl::new(
        Arc::downgrade(&folder_manager),
        Arc::downgrade(&authenticate_user),
      );

      let ai_manager = ChatDepsResolver::resolve(
        Arc::downgrade(&authenticate_user),
        server_provider.clone(),
        store_preference.clone(),
        Arc::downgrade(&storage_manager.storage_service),
        server_provider.clone(),
        folder_query_service.clone(),
        server_provider.local_ai.clone(),
      );

      let database_manager = DatabaseDepsResolver::resolve(
        Arc::downgrade(&authenticate_user),
        task_dispatcher.clone(),
        Arc::downgrade(&collab_builder),
        server_provider.clone(),
        server_provider.clone(),
        ai_manager.clone(),
      )
      .await;

      let document_manager = DocumentDepsResolver::resolve(
        Arc::downgrade(&authenticate_user),
        Arc::downgrade(&collab_builder),
        server_provider.clone(),
        Arc::downgrade(&storage_manager.storage_service),
      );

      let user_manager = UserDepsResolver::resolve(
        authenticate_user.clone(),
        Arc::downgrade(&collab_builder),
        Arc::downgrade(&server_provider),
        store_preference.clone(),
        Arc::downgrade(&database_manager),
        Arc::downgrade(&folder_manager),
      )
      .await;

      let search_manager =
        SearchDepsResolver::resolve(server_provider.clone(), folder_manager.clone()).await;

      // Register the folder operation handlers
      register_handlers(
        &folder_manager,
        Arc::downgrade(&document_manager),
        Arc::downgrade(&database_manager),
        Arc::downgrade(&ai_manager),
      );

      (
        user_manager,
        folder_manager,
        server_provider,
        database_manager,
        document_manager,
        collab_builder,
        search_manager,
        ai_manager,
        storage_manager,
        instant_indexed_data_writer,
      )
    }
    .await;

    let full_indexed_data_writer = Arc::new(RwLock::new(None));
    let (full_indexed_finish_sender, _) = tokio::sync::watch::channel(false);
    let app_life_cycle = AppLifeCycleImpl {
      user_manager: Arc::downgrade(&user_manager),
      collab_builder: Arc::downgrade(&collab_builder),
      folder_manager: Arc::downgrade(&folder_manager),
      database_manager: Arc::downgrade(&database_manager),
      document_manager: Arc::downgrade(&document_manager),
      server_provider: Arc::downgrade(&server_provider),
      storage_manager: Arc::downgrade(&storage_manager),
      ai_manager: Arc::downgrade(&ai_manager),
      search_manager: Arc::downgrade(&search_manager),
      instant_indexed_data_writer,
      full_indexed_data_writer: Arc::downgrade(&full_indexed_data_writer),
      logged_user: Arc::new(ServerUserImpl(Arc::downgrade(&authenticate_user))),
      runtime: runtime.clone(),
      full_indexed_finish_sender,
    };

    let collab_interact_impl = CollabInteractImpl {
      database_manager: Arc::downgrade(&database_manager),
      document_manager: Arc::downgrade(&document_manager),
    };
    if let Err(err) = user_manager
      .init_with_callback(app_life_cycle, collab_interact_impl)
      .await
    {
      error!("Init user failed: {}", err)
    }

    #[allow(clippy::arc_with_non_send_sync)]
    let event_dispatcher = Arc::new(AFPluginDispatcher::new(
      runtime,
      make_plugins(
        Arc::downgrade(&folder_manager),
        Arc::downgrade(&database_manager),
        Arc::downgrade(&user_manager),
        Arc::downgrade(&document_manager),
        Arc::downgrade(&search_manager),
        Arc::downgrade(&ai_manager),
        Arc::downgrade(&storage_manager),
        Arc::new(config.clone()),
      ),
    ));

    Self {
      config,
      user_manager,
      document_manager,
      folder_manager,
      database_manager,
      event_dispatcher,
      server_provider,
      task_dispatcher,
      store_preference,
      search_manager,
      ai_manager,
      storage_manager,
      collab_builder,
      full_indexed_data_writer,
    }
  }

  /// Only expose the dispatcher in test
  pub fn dispatcher(&self) -> Arc<AFPluginDispatcher> {
    self.event_dispatcher.clone()
  }
}

struct ServerUserImpl(Weak<AuthenticateUser>);

impl ServerUserImpl {
  fn upgrade_user(&self) -> Result<Arc<AuthenticateUser>, FlowyError> {
    let user = self
      .0
      .upgrade()
      .ok_or(FlowyError::internal().with_context("Unexpected error: UserSession is None"))?;
    Ok(user)
  }
}

#[async_trait]
impl LoggedUser for ServerUserImpl {
  fn workspace_id(&self) -> FlowyResult<Uuid> {
    self.upgrade_user()?.workspace_id()
  }

  fn workspace_type(&self) -> FlowyResult<WorkspaceType> {
    self.upgrade_user()?.workspace_type()
  }

  fn user_id(&self) -> FlowyResult<i64> {
    self.upgrade_user()?.user_id()
  }

  async fn is_local_mode(&self) -> FlowyResult<bool> {
    self.upgrade_user()?.is_local_mode().await
  }

  fn get_sqlite_db(&self, uid: i64) -> Result<DBConnection, FlowyError> {
    self.upgrade_user()?.get_sqlite_connection(uid)
  }

  fn get_collab_db(&self, uid: i64) -> Result<Weak<CollabKVDB>, FlowyError> {
    self.upgrade_user()?.get_collab_db(uid)
  }

  fn application_root_dir(&self) -> Result<PathBuf, FlowyError> {
    Ok(PathBuf::from(
      self.upgrade_user()?.get_application_root_dir(),
    ))
  }
}
