//! POSIX descriptor walks and mode, owner and link checks, shared by the macOS adapter.

mod private;
mod protected;
mod shell_files;

pub(crate) use private::{FILE_FLAGS, same_entry, user};
pub(crate) use protected::{
    DIRECTORY_FLAGS, components, open_directory, safe_directory, safe_regular, same_inspection,
    stat,
};
pub use shell_files::AcquisitionError;
pub(crate) use shell_files::{
    basename, entry_stat, named, open_at, private_directory, same_object,
};
