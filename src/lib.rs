//! disk-cloner core: imaging, restoring, cloning and writing whole disks.
//! The `disk-cloner` CLI and the optional `disk-cloner-gui` are thin front ends.

pub mod backup;
pub mod checksum;
pub mod clone;
pub mod lsblk;
pub mod manifest;
pub mod pcimage;
pub mod ptable;
pub mod restore;
pub mod shrink;
pub mod util;
pub mod write;
