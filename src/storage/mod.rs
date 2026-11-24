pub use sqrzl_emulator_storage::storage::*;

pub mod filesystem {
    pub use sqrzl_emulator_storage::storage::filesystem::*;

    #[cfg(test)]
    mod consistency_tests;
    #[cfg(test)]
    mod protection_tests;
}
