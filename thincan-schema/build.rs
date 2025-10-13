use flatc_rust;

use std::{
    env, fs,
    path::{Path, PathBuf},
};

const SCHEMAS: &[&str] = &["monster"];

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    for schema in SCHEMAS {
        let schema_path = format!("schema/{schema}.fbs");
        println!("cargo:rerun-if-changed={}", schema_path);

        let mut schema_out_path = out_dir.clone();
        schema_out_path.push("flatbuffers");

        // Generate Rust file for schema using flatc
        flatc_rust::run(flatc_rust::Args {
            inputs: &[Path::new(&schema_path)],
            out_dir: schema_out_path.as_path(),
            ..Default::default()
        })
        .expect("flatc");
    }

    // Generate top-level messages module
    let mut mod_file = String::new();
    for schema in SCHEMAS {
        mod_file.push_str(&format!(
            "#[path = \"flatbuffers/{schema}_generated.rs\"] pub mod {lower};\n",
            lower = schema.to_lowercase(),
        ));
    }
    fs::write(out_dir.join("generated_mods.rs"), mod_file).unwrap();
}
