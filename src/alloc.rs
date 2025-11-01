// Optional global allocator selection. This module is compiled only when the
// corresponding feature is enabled and the dependency is added to Cargo.toml.

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
mod jemalloc_global {
    extern crate jemallocator;
    #[global_allocator]
    static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;
}

#[cfg(all(feature = "mimalloc", not(feature = "jemalloc")))]
mod mimalloc_global {
    use mimalloc::MiMalloc;
    #[global_allocator]
    static GLOBAL: MiMalloc = MiMalloc;
}


