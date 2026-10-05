use super::*;

#[test]
fn every_asset_lnx_downloads_is_pinned_with_a_sha256() {
    let release = image_release();
    assert!(release.starts_with("images-v"), "{release}");
    for asset in ["vmlinuz.gz", "rootfs.ext4.zst", "lnx-linux-aarch64"] {
        let digest = pinned_sha256(release, asset).expect(asset);
        assert_eq!(digest.len(), 64, "{asset}");
        assert!(
            digest.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{asset}: {digest}"
        );
    }
}

#[test]
fn unpinned_assets_and_other_releases_are_refused() {
    assert!(pinned_sha256(image_release(), "something-else").is_err());
    assert!(pinned_sha256("images-v0.0.1", "vmlinuz.gz").is_err());
}

#[test]
fn downloads_must_match_their_pin() {
    let dir = std::env::temp_dir().join(format!("lnx-release-assets-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    let file = dir.join("vmlinuz.gz");
    fs::write(&file, b"not the kernel").expect("write");

    let error = verify_download(&file, image_release(), "vmlinuz.gz")
        .expect_err("a different file is refused")
        .to_string();
    assert!(error.contains("refusing to use it"), "{error}");
    assert!(
        error.contains(&sha256_file(&file).expect("hash")),
        "the error names the actual digest: {error}"
    );
    fs::remove_dir_all(&dir).expect("remove temp dir");
}

#[test]
fn sha256_matches_a_known_digest() {
    let dir = std::env::temp_dir().join(format!("lnx-release-sha-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    let file = dir.join("abc");
    fs::write(&file, b"abc").expect("write");
    assert_eq!(
        sha256_file(&file).expect("hash"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    fs::remove_dir_all(&dir).expect("remove temp dir");
}
