fn main() {
    // Tests and examples find libcef next to themselves (cef-dll-sys copies
    // it into target/<profile>), or one level up from target/<profile>/deps
    // and target/<profile>/examples.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // Only this package's own binaries (tests, examples) get it.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN:$ORIGIN/..");
    }
}
