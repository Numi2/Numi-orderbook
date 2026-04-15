// Optional global allocator selection. jemalloc is preferred on Linux when both
// allocator features are enabled; mimalloc remains available elsewhere.

#[cfg(all(target_os = "linux", feature = "jemalloc"))]
mod jemalloc_global {
    extern crate jemallocator;
    #[global_allocator]
    static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;
}

#[cfg(all(
    feature = "mimalloc",
    any(not(feature = "jemalloc"), not(target_os = "linux"))
))]
mod mimalloc_global {
    use mimalloc::MiMalloc;
    #[global_allocator]
    static GLOBAL: MiMalloc = MiMalloc;
}
