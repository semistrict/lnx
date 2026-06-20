use super::*;

#[test]
fn parses_subnet_specs() {
    let (subnet, prefix) = parse_subnet("192.168.106.0/24").expect("parse");
    assert_eq!(subnet, Ipv4Addr::new(192, 168, 106, 0));
    assert_eq!(prefix, 24);

    assert!(parse_subnet("192.168.106.0").is_err());
    assert!(parse_subnet("192.168.106.1/24").is_err());
    assert!(parse_subnet("192.168.106.0/31").is_err());
    assert!(parse_subnet("bogus/24").is_err());
}

#[test]
fn computes_mask_and_gateway() {
    assert_eq!(mask_for_prefix(24), Ipv4Addr::new(255, 255, 255, 0));
    assert_eq!(mask_for_prefix(16), Ipv4Addr::new(255, 255, 0, 0));
    assert_eq!(
        gateway_for_subnet(Ipv4Addr::new(192, 168, 106, 0)),
        Ipv4Addr::new(192, 168, 106, 0)
    );
}

#[test]
fn vmnet_configuration_uses_gateway_network_address() {
    let subnet = Ipv4Addr::new(192, 168, 106, 0);
    let gateway = gateway_for_subnet(subnet);

    assert_eq!(in_addr(subnet).s_addr, u32::from(subnet).to_be());
    assert_eq!(in_addr(subnet).s_addr, u32::from(gateway).to_be());
}
