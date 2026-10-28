//! Arbitrary router answers through the NAT-PMP, SSDP, HTTP and UPnP parsers.
#![no_main]

use dravity_portmap::{Url, natpmp, upnp};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = natpmp::Response::decode(data);
    let _ = upnp::parse_search_response(data);
    let _ = dravity_portmap::fuzz_http_response(data);
    let _ = upnp::check_response(500, data);
    let text = String::from_utf8_lossy(data);
    let _ = upnp::parse_external_ip(&text);
    if let Some(base) = Url::parse("http://192.168.1.1:5000/rootDesc.xml") {
        if let Some(service) = upnp::parse_description(&text, &base) {
            let _ = upnp::get_external_ip(&service);
        }
        let _ = base.join(&text);
    }
    let _ = Url::parse(&text);
});
