//! Compiles the vendored Signalsmith Stretch (header-only C++) behind our C wrapper.
//! Only a C++14 compiler is needed. The C++ is always optimised: unoptimised, it runs
//! up to 10x slower, too slow for the audio thread even in debug builds.

fn main() {
    println!("cargo::rerun-if-changed=src/wrapper.cpp");
    println!("cargo::rerun-if-changed=../../third_party/signalsmith-stretch");
    println!("cargo::rerun-if-changed=../../third_party/signalsmith-linear");
    cc::Build::new()
        .cpp(true)
        .std("c++14")
        .file("src/wrapper.cpp")
        .include("../../third_party")
        .include("../../third_party/signalsmith-stretch")
        .opt_level(2)
        .debug(false)
        .warnings(false)
        .compile("dj_stretch");
}
