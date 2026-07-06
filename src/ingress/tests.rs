use super::*;

fn test_config(http_addr: &str, https_addr: &str, dns_addr: &str, resolver_dir: &str) -> Config {
    Config {
        domain: "lnx".to_string(),
        dns_addr: dns_addr.to_string(),
        http_addr: http_addr.to_string(),
        https_addr: https_addr.to_string(),
        resolver_dir: PathBuf::from(resolver_dir),
        state_dir: PathBuf::from("/tmp/lnx-ingress-test"),
    }
}

fn status_with_binary(binary_path: Option<&str>) -> Status {
    Status {
        domain: "lnx".to_string(),
        dns_addr: "127.0.0.1:5354".to_string(),
        http_addr: "127.0.0.1:80".to_string(),
        https_addr: "127.0.0.1:443".to_string(),
        resolver_path: "/etc/resolver/lnx".to_string(),
        network: None,
        network_error: None,
        protocol_version: Some(PROTOCOL_VERSION),
        binary_path: binary_path.map(str::to_string),
    }
}

#[test]
fn parses_ingress_hosts() {
    let route = parse_host("p8080-dev.lnx", "lnx").expect("parse");
    assert_eq!(route.instance, "dev");
    assert_eq!(route.port, 8080);

    let route = parse_host("p3000-parent-child.lnx:80", "lnx").expect("parse");
    assert_eq!(route.instance, "parent-child");
    assert_eq!(route.port, 3000);

    assert!(parse_host("p0-dev.lnx", "lnx").is_err());
    assert!(parse_host("8080-dev.lnx", "lnx").is_err());
    assert!(parse_host("p8080.lnx", "lnx").is_err());
    assert!(parse_host("p8080.dev.lnx", "lnx").is_err());
}

#[test]
fn extracts_instance_from_bare_hosts() {
    assert_eq!(
        instance_from_host("dev.lnx", "lnx"),
        Some("dev".to_string())
    );
    assert_eq!(
        instance_from_host("Dev.LNX:443", "lnx"),
        Some("dev".to_string())
    );
    assert_eq!(instance_from_host("a.dev.lnx", "lnx"), None);
    assert_eq!(instance_from_host(".lnx", "lnx"), None);
    assert_eq!(instance_from_host("dev.local", "lnx"), None);
}

#[test]
fn parses_attach_requests() {
    assert_eq!(
        attach_instance_from_request(
            "POST /network/attach?instance=dev-1 HTTP/1.1\r\nHost: localhost\r\n\r\n"
        ),
        Some("dev-1".to_string())
    );
    assert_eq!(
        attach_instance_from_request("POST /network/attach?instance= HTTP/1.1\r\n\r\n"),
        None
    );
    assert_eq!(
        attach_instance_from_request("POST /network/attach?instance=../etc HTTP/1.1\r\n\r\n"),
        None
    );
    assert_eq!(
        attach_instance_from_request("GET /status HTTP/1.1\r\n\r\n"),
        None
    );
}

#[test]
fn parses_json_number_fields() {
    assert_eq!(
        json_number_field("{\"prefix\":24,\"x\":\"y\"}", "prefix"),
        Some("24".to_string())
    );
    assert_eq!(json_number_field("{\"prefix\":\"24\"}", "prefix"), None);
    assert_eq!(json_number_field("{}", "prefix"), None);
}

#[test]
fn escapes_status_json_strings() {
    assert_eq!(json_escape("plain"), "plain");
    assert_eq!(json_escape("quote\"slash\\"), "quote\\\"slash\\\\");
    assert_eq!(json_escape("line\n tab\t"), "line\\n tab\\t");
}

#[test]
fn privileged_launchd_uses_system_helper() {
    let config = test_config(
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:5354",
        "/etc/resolver",
    );

    let plist = launchd_plist(&config).expect("plist");

    assert!(plist.contains(SYSTEM_HELPER_PATH));
}

#[test]
fn unprivileged_launchd_uses_current_executable() {
    let config = test_config(
        "127.0.0.1:8080",
        "127.0.0.1:8443",
        "127.0.0.1:5354",
        "/tmp/lnx-resolver-test",
    );
    let exe = std::env::current_exe()
        .expect("current exe")
        .display()
        .to_string();

    let plist = launchd_plist(&config).expect("plist");

    assert!(plist.contains(&xml_escape(&exe)));
    assert!(!plist.contains(SYSTEM_HELPER_PATH));
}

#[test]
fn privileged_service_rejects_user_home_binary() {
    let config = test_config(
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:5354",
        "/etc/resolver",
    );
    let status = status_with_binary(Some("/Users/test/.cargo/bin/lnx"));

    let error = privileged_service_binary_error(&config, &status).expect("error");

    assert!(error.contains("/Users/test/.cargo/bin/lnx"));
    assert!(error.contains(SYSTEM_HELPER_PATH));
}

#[test]
fn privileged_service_accepts_system_helper() {
    let config = test_config(
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:5354",
        "/etc/resolver",
    );
    let status = status_with_binary(Some(SYSTEM_HELPER_PATH));

    assert!(privileged_service_binary_error(&config, &status).is_none());
}

#[test]
fn privileged_service_accepts_matching_helper_contents() {
    let config = test_config(
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:5354",
        "/etc/resolver",
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let current = dir.path().join("current");
    let helper = dir.path().join("helper");
    fs::write(&current, b"same").expect("write current");
    fs::write(&helper, b"same").expect("write helper");

    assert!(
        privileged_service_helper_stale_error_with_paths(&config, &current, &helper)
            .expect("stale check")
            .is_none()
    );
}

#[test]
fn privileged_service_rejects_stale_helper_contents() {
    let config = test_config(
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:5354",
        "/etc/resolver",
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let current = dir.path().join("current");
    let helper = dir.path().join("helper");
    fs::write(&current, b"new").expect("write current");
    fs::write(&helper, b"old").expect("write helper");

    let error = privileged_service_helper_stale_error_with_paths(&config, &current, &helper)
        .expect("stale check")
        .expect("error");

    assert!(error.contains("ingress system helper is stale"));
    assert!(error.contains(&helper.display().to_string()));
    assert!(error.contains(&current.display().to_string()));
}

#[test]
fn privileged_service_status_reports_stale_helper_contents() {
    let config = test_config(
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:5354",
        "/etc/resolver",
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let current = dir.path().join("current");
    let helper = dir.path().join("helper");
    fs::write(&current, b"new").expect("write current");
    fs::write(&helper, b"old").expect("write helper");
    let mut status = status_with_binary(Some(SYSTEM_HELPER_PATH));
    status.binary_path = Some(helper.display().to_string());

    let helper_status = privileged_service_status_with_paths(&config, &status, &current, &helper)
        .expect("helper status");

    assert_eq!(
        helper_status.as_deref(),
        Some("stale; run `sudo lnx ingress enable` from a terminal")
    );
}

#[test]
fn privileged_service_status_reports_current_helper_contents() {
    let config = test_config(
        "127.0.0.1:80",
        "127.0.0.1:443",
        "127.0.0.1:5354",
        "/etc/resolver",
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let current = dir.path().join("current");
    let helper = dir.path().join("helper");
    fs::write(&current, b"same").expect("write current");
    fs::write(&helper, b"same").expect("write helper");
    let mut status = status_with_binary(Some(SYSTEM_HELPER_PATH));
    status.binary_path = Some(helper.display().to_string());

    let helper_status = privileged_service_status_with_paths(&config, &status, &current, &helper)
        .expect("helper status");

    assert_eq!(helper_status.as_deref(), Some("current"));
}

fn dns_query(host: &str) -> Vec<u8> {
    let mut packet = vec![
        0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    for label in host.split('.') {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&1u16.to_be_bytes());
    packet.extend_from_slice(&1u16.to_be_bytes());
    packet
}

#[test]
fn dns_answers_port_hosts_with_localhost_and_instances_with_their_ip() {
    let mut ips = HashMap::new();
    ips.insert("dev".to_string(), Ipv4Addr::new(192, 168, 106, 2));

    let response = dns_response(&dns_query("p8080-dev.lnx"), "lnx", &ips).expect("response");
    assert_eq!(&response[response.len() - 4..], &[127, 0, 0, 1]);

    let response = dns_response(&dns_query("dev.lnx"), "lnx", &ips).expect("response");
    assert_eq!(&response[response.len() - 4..], &[192, 168, 106, 2]);

    // NXDOMAIN: rcode 3, no answer records.
    let response = dns_response(&dns_query("other.lnx"), "lnx", &ips).expect("response");
    assert_eq!(response[3], 0x03);
    assert_eq!(&response[6..8], &[0, 0]);
}

#[test]
fn parses_fork_query_from_request_target() {
    assert_eq!(
        fork_source_from_request_target("/vnc.html?lnx:fork=foo"),
        Some("foo".to_string())
    );
    assert_eq!(
        fork_source_from_request_target("/vnc.html?a=1&lnx:fork=source%2Evm&b=2"),
        Some("source.vm".to_string())
    );
    assert_eq!(fork_source_from_request_target("/vnc.html?a=1"), None);
    assert_eq!(fork_source_from_request_target("/vnc.html?lnx:fork="), None);
}

#[test]
fn removes_only_fork_query_param_for_redirect() {
    assert_eq!(clean_fork_request_target("/?lnx:fork=foo"), "/");
    assert_eq!(
        clean_fork_request_target("/vnc.html?a=1&lnx:fork=foo&b=2"),
        "/vnc.html?a=1&b=2"
    );
    assert_eq!(
        clean_fork_request_target("/vnc.html?autoconnect=true"),
        "/vnc.html?autoconnect=true"
    );
}

#[test]
fn extracts_request_target() {
    assert_eq!(
        request_target(b"GET /vnc.html?lnx:fork=foo HTTP/1.1\r\nHost: p6080.bar.lnx\r\n\r\n"),
        Some("/vnc.html?lnx:fork=foo")
    );
}

#[test]
fn rewrites_chrome_devtools_host_header() {
    let request =
        b"GET /json/version HTTP/1.1\r\nHost: p9222-default.lnx\r\nConnection: close\r\n\r\n"
            .to_vec();

    assert_eq!(
        rewrite_proxy_request_host(request, 9222),
        b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:9222\r\nConnection: close\r\n\r\n".to_vec()
    );
}

#[test]
fn preserves_other_proxy_host_headers() {
    let request = b"GET / HTTP/1.1\r\nHost: p6080-default.lnx\r\n\r\n".to_vec();

    assert_eq!(rewrite_proxy_request_host(request.clone(), 6080), request);
}

#[test]
fn generated_ca_is_name_constrained_to_the_ingress_domain() {
    let state_dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        domain: "lnx".to_string(),
        dns_addr: "127.0.0.1:5354".to_string(),
        http_addr: "127.0.0.1:8080".to_string(),
        https_addr: "127.0.0.1:8443".to_string(),
        resolver_dir: state_dir.path().join("resolver"),
        state_dir: state_dir.path().to_path_buf(),
    };
    fs::create_dir_all(config.ca_dir()).expect("create ca dir");

    generate_ca(&config).expect("generate ca");

    let output = Command::new("openssl")
        .arg("x509")
        .arg("-in")
        .arg(config.ca_cert_path())
        .arg("-text")
        .arg("-noout")
        .output()
        .expect("read ca cert");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("utf8 cert text");

    assert!(text.contains("X509v3 Name Constraints: critical"), "{text}");
    assert!(text.contains("Permitted:"), "{text}");
    assert!(text.contains("DNS:.lnx"), "{text}");
    assert!(text.contains("DNS:lnx"), "{text}");
    assert!(text.contains("Excluded:"), "{text}");
    assert!(text.contains("IP:0.0.0.0/0.0.0.0"), "{text}");
    assert!(
        text.contains("X509v3 Basic Constraints: critical"),
        "{text}"
    );
    assert!(text.contains("CA:TRUE, pathlen:0"), "{text}");
    assert!(text.contains("Certificate Sign, CRL Sign"), "{text}");
}
