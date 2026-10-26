//! The `wt` binary. It is the only crate compiled with the release version and
//! build identity, so a release or new commit recompiles this shim alone and the
//! application library comes from cache (docs/ci.md).

fn main() {
    wt::main(wt::BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        build_id: env!("WT_BUILD_ID"),
        target: env!("WT_TARGET"),
    });
}
