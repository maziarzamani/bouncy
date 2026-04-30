//! Build-time V8 startup snapshot generator.
//!
//! Boots a snapshot-creator isolate, evaluates `js/bootstrap.js` into the
//! default context, freezes the result into a `StartupData` blob, and
//! writes it to `$OUT_DIR/BOUNCY_SNAPSHOT.bin`. The lib's runtime then
//! `include_bytes!`-loads that blob and feeds it to every new isolate via
//! `CreateParams::snapshot_blob(...)`.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=js/bootstrap.js");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let snapshot_path = out_dir.join("BOUNCY_SNAPSHOT.bin");

    let bootstrap_js = include_str!("js/bootstrap.js");

    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();

    let blob = {
        let mut snapshot_creator = v8::Isolate::snapshot_creator(None, None);
        {
            v8::scope!(let scope, &mut snapshot_creator);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);

            let code = v8::String::new(scope, bootstrap_js).expect("bootstrap.js to v8 string");
            let script =
                v8::Script::compile(scope, code, None).expect("bootstrap.js compile (build.rs)");
            script.run(scope).expect("bootstrap.js run (build.rs)");

            scope.set_default_context(context);
        }
        snapshot_creator
            .create_blob(v8::FunctionCodeHandling::Keep)
            .expect("create_blob")
    };

    std::fs::write(&snapshot_path, &*blob).expect("write snapshot");
    println!(
        "cargo:rustc-env=BOUNCY_SNAPSHOT_PATH={}",
        snapshot_path.display()
    );
}
