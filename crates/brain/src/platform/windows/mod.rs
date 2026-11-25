//! Win32 implementations of the brain's platform functions.

mod capture;
mod clock;
mod credential;
mod dump;
mod elements;
#[cfg(feature = "test-support")]
pub use elements::measurement::{WindowProbe, WindowSample, WindowSession};
#[cfg(test)]
pub(crate) mod fake_claude;
mod fault;
mod folders;
mod fs;
mod job;
mod library;
#[expect(dead_code, reason = "A6 opens OCR; tests never run native recognition")]
pub(crate) mod ocr;
mod pipe;
mod policy;
mod process;
mod protected;
mod root_store;
mod spawn;
mod window;

pub(crate) use super::Untrusted;
pub(crate) use capture::{WINDOW_CAPTURE, display_at, screen_capturer};
#[cfg(any(debug_assertions, feature = "gate-draw-replay"))]
pub(crate) use clock::qpc_now;
pub(crate) use clock::{qpc_age, qpc_at, qpc_left};
pub(crate) use credential::WindowsCredentialStore;
pub use credential::{delete_instance, store_instance};
pub(crate) use dump::DumpExclusion;
pub(crate) use elements::screen_elements;
#[cfg(debug_assertions)]
pub(crate) use fault::call_throwing_export;
pub use fault::quiet_failures;
#[cfg(debug_assertions)]
pub use fault::raise_access_violation;
pub use folders::{config_dir, data_dir, session_lock_path};
pub use fs::{free_space, links_unsupported, replace_file};
pub use job::{Helper, Job};
pub(super) use library::{
    Library, RawExport, missing_dependency, modules, open_locked, system_dir,
};
#[cfg(test)]
pub(crate) use pipe::test_pair;
pub(crate) use pipe::{PipeClient, bridge_peer, open_bridge, shell_connection};
pub use pipe::{
    PipeServer, client_image, client_process, create_owner_only, has_exited, process_image,
    server_image, session_id,
};
pub use policy::{managed_presence, system_env};
pub use process::{detached_command, launch_detached};
#[cfg(test)]
pub(super) use protected::final_path_matches;
#[cfg(test)]
pub(crate) use protected::{
    DaclEntry, dacl_entries, dacl_sddl, pause_after_check, paused, set_dacl, this_user,
};
pub(crate) use protected::{
    check_model_final, create_model_dir, create_model_write, discard_model, hold_chain, hold_file,
    hold_model_creation, local_app_data, mark, marker, model_file, model_final, open_model_write,
    owner_only_dir, private_tree, profile_dir, publish_model, remove_model_tree, remove_tree,
    unmark, validate_model_path,
};
pub use root_store::trust_roots;
pub use spawn::spawn;
pub(crate) use spawn::spawn_if;
#[cfg(test)]
pub(crate) use spawn::{SpawnEdge, observe_next_spawn};
pub(crate) use window::unchanged as window_unchanged;
