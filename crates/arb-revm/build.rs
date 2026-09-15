fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Export `__revmc_builtin_*` from test/bin links so JIT-compiled code can
    // resolve builtins. No-op when the compiled-frame feature is off.
    #[cfg(feature = "compiled-frame")]
    revmc_build::emit();
}
