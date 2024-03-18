// use ldap_proxy::proxy::BasicLdapClient;

use ldap3_proto::proto::LdapResult;
use ldap_proxy::proxy::CachedValue;
use ldap_proxy::Config;
use std::time::{Duration, Instant};

#[test]
fn hello_world() {
    assert_eq!(2 + 2, 4);
}

#[test]
fn test_config_load() {
    assert!(toml::from_str::<Config>("").is_err());

    assert!(toml::from_str::<Config>(include_str!("test_config.toml")).is_ok());
    let config = toml::from_str::<Config>(include_str!("test_config.toml")).unwrap();

    assert_eq!(config.ldap_ca.to_str(), Some("/etc/ldap-proxy/ldap-ca.pem"));
}

#[test]
fn test_cachedvalue() {
    let cv = CachedValue {
        valid_until: Instant::now() + Duration::from_secs(60),
        entries: Vec::with_capacity(5),
        result: LdapResult {
            code: ldap3_proto::LdapResultCode::Busy,
            matcheddn: "dn=doo".to_string(),
            message: "ohno".to_string(),
            referral: Vec::with_capacity(5),
        },
        ctrl: Vec::with_capacity(5),
    };
    assert_eq!(cv.size(), 144);
}
