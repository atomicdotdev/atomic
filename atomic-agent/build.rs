fn main() {
    // libgit2 1.8 (libgit2-sys 0.17.0+1.8.1) references Advapi32 symbols
    // (GetNamedSecurityInfoW, RegOpenKeyExW, RegQueryValueExW, RegCloseKey)
    // but its build script never links Advapi32 on MSVC. Emitted from this
    // crate's own build script so every binary and test binary of this
    // crate resolves them (a wrapper crate's flags are dropped from the
    // link graph when nothing references it).
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
