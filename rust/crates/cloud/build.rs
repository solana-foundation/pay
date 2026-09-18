use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let asset_dir = out_dir.join("cloud-dist");

    // Workspace root: crates/cloud/../../ = rust/, then ../web-ui/dist-cloud
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_dist = manifest_dir.join("../../../web-ui/dist-cloud");

    println!("cargo:rerun-if-env-changed=PAY_CLOUD_DIST");
    println!("cargo:rerun-if-env-changed=PAY_CLOUD_ALLOW_PLACEHOLDER");
    println!("cargo:rerun-if-changed={}", repo_dist.display());
    print_rerun_if_changed_recursive(&repo_dist);

    let cloud_dist = resolve_cloud_dist(&repo_dist);
    let Some(cloud_dist) = cloud_dist else {
        if env::var("PROFILE").as_deref() == Ok("release")
            && env::var_os("PAY_CLOUD_ALLOW_PLACEHOLDER").is_none()
        {
            panic!(
                "web-ui/dist-cloud assets not found. Build the UI with `cd web-ui && pnpm install --frozen-lockfile && pnpm build:cloud`, \
                 set PAY_CLOUD_DIST to a built dist-cloud directory, or set PAY_CLOUD_ALLOW_PLACEHOLDER=1 \
                 to embed the placeholder."
            );
        }
        println!(
            "cargo:warning=web-ui/dist-cloud assets not found at {}, embedding empty placeholder",
            repo_dist.display()
        );
        fs::create_dir_all(&asset_dir).unwrap();
        fs::write(
            asset_dir.join("index.html"),
            "<html><body><h1>Pay</h1><p>Run <code>pnpm build:cloud</code> in <code>web-ui/</code> to build the onboarding UI.</p></body></html>",
        )
        .unwrap();
        return;
    };

    if asset_dir.exists() {
        fs::remove_dir_all(&asset_dir).unwrap();
    }
    copy_dir_recursive(&cloud_dist, &asset_dir);
    print_rerun_if_changed_recursive(&cloud_dist);

    println!(
        "cargo:warning=Embedded web-ui/dist-cloud from {}",
        cloud_dist
            .canonicalize()
            .unwrap_or_else(|_| cloud_dist.to_path_buf())
            .display()
    );
}

fn resolve_cloud_dist(repo_dist: &Path) -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("PAY_CLOUD_DIST") {
        let explicit = PathBuf::from(explicit);
        if explicit.join("index.html").is_file() {
            println!("cargo:rerun-if-changed={}", explicit.display());
            print_rerun_if_changed_recursive(&explicit);
            return Some(explicit);
        }
        panic!(
            "PAY_CLOUD_DIST must point to a built dist-cloud directory with index.html: {}",
            explicit.display()
        );
    }

    repo_dist
        .join("index.html")
        .is_file()
        .then(|| repo_dist.to_path_buf())
}

fn print_rerun_if_changed_recursive(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };

    if metadata.is_file() {
        println!("cargo:rerun-if-changed={}", path.display());
        return;
    }

    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        print_rerun_if_changed_recursive(&entry.path());
    }
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
