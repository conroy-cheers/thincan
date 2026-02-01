use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn run(dest_path: &Path) -> io::Result<()> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let schema_dir = manifest_dir.join("tests/support/schema");
    let schema_file = schema_dir.join("person.capnp");

    let out_dir = std::env::temp_dir().join(format!("thincan_capnp_gen_{}", std::process::id()));
    if out_dir.exists() {
        fs::remove_dir_all(&out_dir)?;
    }
    fs::create_dir_all(&out_dir)?;

    capnpc::CompilerCommand::new()
        .src_prefix(&schema_dir)
        .file(&schema_file)
        .output_path(&out_dir)
        .run()
        .map_err(|e| io::Error::other(e.to_string()))?;

    let generated_path = out_dir.join("person_capnp.rs");
    let generated = fs::read_to_string(&generated_path)
        .map_err(|e| io::Error::new(e.kind(), format!("failed to read {generated_path:?}: {e}")))?;

    fs::write(dest_path, generated.as_bytes())?;

    println!("Wrote {}", dest_path.display());
    Ok(())
}

fn main() -> io::Result<()> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let default_dest: PathBuf = manifest_dir.join("tests/support/person_capnp.rs");
    let dest_path = std::env::var_os("THINCAN_CAPNP_DEST")
        .map(PathBuf::from)
        .unwrap_or(default_dest);
    run(&dest_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_writes_output_to_custom_destination() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "thincan_capnp_gen_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir)?;
        let dest = dir.join("person_capnp.rs");

        run(&dest)?;
        // Exercise the cleanup branch which removes the previous generated directory.
        run(&dest)?;

        // Cover the binary entrypoint without writing into the repository.
        let main_dest = dir.join("person_capnp_main.rs");
        unsafe { std::env::set_var("THINCAN_CAPNP_DEST", &main_dest) };
        let main_res = main();
        unsafe { std::env::remove_var("THINCAN_CAPNP_DEST") };
        main_res?;

        let contents = fs::read_to_string(&dest)?;
        assert!(contents.contains("pub mod person"));
        Ok(())
    }
}
