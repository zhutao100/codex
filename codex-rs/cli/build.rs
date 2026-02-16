mod shared {
    include!("../build.rs");

    pub fn run() {
        main();
    }
}

fn main() {
    // Keep this build script centralized at the workspace root while still letting
    // Cargo rerun when the shared implementation changes.
    println!("cargo:rerun-if-changed=../build.rs");
    shared::run();
}
