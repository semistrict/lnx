use super::*;

#[test]
fn parses_image_references() {
    let alpine = ImageReference::parse("alpine:3.21").expect("alpine");
    assert_eq!(alpine.registry, "docker.io");
    assert_eq!(alpine.repository, "library/alpine");
    assert_eq!(alpine.reference, "3.21");

    let bare = ImageReference::parse("ubuntu").expect("bare");
    assert_eq!(bare.repository, "library/ubuntu");
    assert_eq!(bare.reference, "latest");

    let ghcr = ImageReference::parse("ghcr.io/owner/tool:v1.2").expect("ghcr");
    assert_eq!(ghcr.registry, "ghcr.io");
    assert_eq!(ghcr.repository, "owner/tool");
    assert_eq!(ghcr.reference, "v1.2");

    let digest = ImageReference::parse("alpine@sha256:abc123").expect("digest");
    assert_eq!(digest.repository, "library/alpine");
    assert_eq!(digest.reference, "sha256:abc123");

    let port = ImageReference::parse("localhost:5000/img:dev").expect("port");
    assert_eq!(port.registry, "localhost:5000");
    assert_eq!(port.repository, "img");
    assert_eq!(port.reference, "dev");
}
