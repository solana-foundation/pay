use std::{env, fs, path::PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let asset_dir = out_dir.join("pdb-dist");

    // Workspace root: crates/pdb/../../ = rust/, then ../pdb/dist
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let pdb_dist = manifest_dir.join("../../../pdb/dist");

    if !pdb_dist.exists() {
        println!(
            "cargo:warning=pdb/dist not found at {}, embedding empty placeholder",
            pdb_dist.display()
        );
        fs::create_dir_all(&asset_dir).unwrap();
        fs::write(
            asset_dir.join("index.html"),
            "<html><body><h1>Payment Debugger</h1><p>Run <code>pnpm build</code> in <code>pdb/</code> to build the UI.</p></body></html>",
        ).unwrap();
        return;
    }

    // Re-run if dist contents change.
    println!("cargo:rerun-if-changed={}", pdb_dist.display());

    if asset_dir.exists() {
        fs::remove_dir_all(&asset_dir).unwrap();
    }
    copy_dir_recursive(&pdb_dist, &asset_dir);

    println!(
        "cargo:warning=Embedded pdb/dist from {}",
        pdb_dist.canonicalize().unwrap_or(pdb_dist).display()
    );
}

fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).unwrap();
        }
    }
}
