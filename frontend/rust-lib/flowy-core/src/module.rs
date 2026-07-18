use flowy_ai::ai_manager::AIManager;
use std::sync::{Arc, Weak};

use flowy_database2::DatabaseManager;
use flowy_document::manager::DocumentManager as DocumentManager2;
use flowy_folder::manager::FolderManager;
use flowy_search::services::manager::SearchManager;
use flowy_storage::manager::StorageManager;
use flowy_user::user_manager::UserManager;
use lib_dispatch::prelude::AFPlugin;

use crate::backup_event;
use crate::config::AppFlowyCoreConfig;

pub fn make_plugins(
  folder_manager: Weak<FolderManager>,
  database_manager: Weak<DatabaseManager>,
  user_session: Weak<UserManager>,
  document_manager2: Weak<DocumentManager2>,
  search_manager: Weak<SearchManager>,
  ai_manager: Weak<AIManager>,
  file_storage_manager: Weak<StorageManager>,
  config: Arc<AppFlowyCoreConfig>,
) -> Vec<AFPlugin> {
  let user_plugin = flowy_user::event_map::init(user_session.clone());
  let folder_plugin = flowy_folder::event_map::init(folder_manager.clone());
  let database_plugin = flowy_database2::event_map::init(database_manager.clone());
  let document_plugin2 = flowy_document::event_map::init(document_manager2.clone());
  let date_plugin = flowy_date::event_map::init();
  let search_plugin = flowy_search::event_map::init(search_manager);
  let ai_plugin = flowy_ai::event_map::init(ai_manager);
  let file_storage_plugin = flowy_storage::event_map::init(file_storage_manager.clone());
  let backup_plugin = backup_event::init(
    user_session,
    folder_manager,
    database_manager,
    document_manager2,
    file_storage_manager,
    config,
  );
  vec![
    user_plugin,
    folder_plugin,
    database_plugin,
    document_plugin2,
    date_plugin,
    search_plugin,
    ai_plugin,
    file_storage_plugin,
    backup_plugin,
  ]
}
