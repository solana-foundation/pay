use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let asset_dir = out_dir.join("pdb-dist");

    // Workspace root: crates/pdb/../../ = rust/, then ../pdb/dist
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_dist = manifest_dir.join("../../../pdb/dist");

    println!("cargo:rerun-if-env-changed=PAY_PDB_DIST");
    println!("cargo:rerun-if-env-changed=PAY_PDB_ALLOW_PLACEHOLDER");
    println!("cargo:rerun-if-changed={}", repo_dist.display());

    let pdb_dist = resolve_pdb_dist(&repo_dist);
    let Some(pdb_dist) = pdb_dist else {
        if env::var("PROFILE").as_deref() == Ok("release")
            && env::var_os("PAY_PDB_ALLOW_PLACEHOLDER").is_none()
        {
            panic!(
                "pdb/dist assets not found. Build the UI with `cd pdb && pnpm install --frozen-lockfile && pnpm build`, \
                 unpack the release PDB artifact and set PAY_PDB_DIST to its dist directory, or set PAY_PDB_ALLOW_PLACEHOLDER=1 \
                 to embed the placeholder."
            );
        }
        println!(
            "cargo:warning=pdb/dist assets not found at {}, embedding empty placeholder",
            repo_dist.display()
        );
        fs::create_dir_all(&asset_dir).unwrap();
        fs::write(
            asset_dir.join("index.html"),
            "<html><body><h1>Payment Debugger</h1><p>Run <code>pnpm build</code> in <code>pdb/</code> to build the UI.</p></body></html>",
        )
        .unwrap();
        return;
    };

    if asset_dir.exists() {
        fs::remove_dir_all(&asset_dir).unwrap();
    }
    copy_dir_recursive(&pdb_dist, &asset_dir);

    println!(
        "cargo:warning=Embedded pdb/dist from {}",
        pdb_dist
            .canonicalize()
            .unwrap_or_else(|_| pdb_dist.to_path_buf())
            .display()
    );
}

fn resolve_pdb_dist(repo_dist: &Path) -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("PAY_PDB_DIST") {
        let explicit = PathBuf::from(explicit);
        if explicit.join("index.html").is_file() {
            println!("cargo:rerun-if-changed={}", explicit.display());
            return Some(explicit);
        }
        panic!(
            "PAY_PDB_DIST must point to a built PDB dist directory with index.html: {}",
            explicit.display()
        );
    }

    repo_dist
        .join("index.html")
        .is_file()
        .then(|| repo_dist.to_path_buf())
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
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
