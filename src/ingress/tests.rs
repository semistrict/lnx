use super::*;

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
#[cfg(target_os = "macos")]
fn derives_vmnet_gateway_from_assigned_ip() {
    assert_eq!(
        gateway_for_assigned_ip(Ipv4Addr::new(192, 168, 106, 207), 24),
        Some(Ipv4Addr::new(192, 168, 106, 0))
    );
    assert_eq!(
        gateway_for_assigned_ip(Ipv4Addr::new(10, 42, 19, 10), 16),
        Some(Ipv4Addr::new(10, 42, 0, 0))
    );
    assert_eq!(
        gateway_for_assigned_ip(Ipv4Addr::new(10, 0, 0, 2), 31),
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
