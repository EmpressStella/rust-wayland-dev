use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let source = manifest_dir.join("src/calendar_query.c");
    let output_dir = PathBuf::from(env::var("OUT_DIR").expect("out dir"));
    let output = output_dir.join("calendar-query");

    let eds = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libecal-2.0")
        .expect("failed to probe libecal-2.0");

    let mut command = Command::new("cc");
    command.arg(&source).arg("-o").arg(&output);

    for include_path in eds.include_paths {
        command.arg("-I").arg(include_path);
    }

    command.args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-O2", "-g0"]);

    for link_path in eds.link_paths {
        command.arg("-L").arg(link_path);
    }

    for lib in eds.libs {
        command.arg(format!("-l{lib}"));
    }

    // libecal pulls in libedataserver and friends, but keep the direct linkage explicit.
    command.args(["-lglib-2.0", "-lgobject-2.0", "-lgio-2.0", "-lm"]);

    let status = command.status().expect("failed to spawn cc");
    assert!(status.success(), "failed to compile calendar-query helper");

    println!("cargo:rerun-if-changed={}", source.display());
    println!(
        "cargo:rustc-env=SIDEBAR_CALENDAR_QUERY_HELPER={}",
        output.display()
    );
}
