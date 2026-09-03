//! Fetching and parsing UPnP device descriptions.
//!
//! Both the receiver and the TV publish one, and it is the only identity source
//! that works in every power state — a Samsung in standby keeps its UPnP stack
//! alive long after it has closed its control ports.

use std::net::IpAddr;

use crate::Result;

/// The identity fields of a UPnP device description.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Description {
    pub friendly_name: Option<String>,
    pub model_name: Option<String>,
    pub model_number: Option<String>,
    pub manufacturer: Option<String>,
    pub serial_number: Option<String>,
    /// Unique Device Name, e.g. `uuid:09896800-00a0-1000-adfd-e47dbd7a6128`.
    pub udn: Option<String>,
}

impl Description {
    /// True when the manufacturer contains `needle`, case-insensitively.
    pub fn made_by(&self, needle: &str) -> bool {
        self.manufacturer
            .as_deref()
            .is_some_and(|m| m.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()))
    }

    /// The UDN with its `uuid:` prefix removed.
    pub fn uuid(&self) -> Option<&str> {
        let udn = self.udn.as_deref()?;
        Some(udn.strip_prefix("uuid:").unwrap_or(udn))
    }
}

/// Parse a UPnP description document.
///
/// Takes the *first* occurrence of each field: descriptions nest a `deviceList`
/// of sub-devices whose names are less useful than the root device's.
pub fn parse(xml: &str) -> Option<Description> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let first = |tag: &str| -> Option<String> {
        doc.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let description = Description {
        // Some Denon firmware prefixes the model with a marker that is not part
        // of the name.
        model_name: first("modelName").map(|m| m.trim_start_matches('*').trim().to_string()),
        friendly_name: first("friendlyName"),
        model_number: first("modelNumber"),
        manufacturer: first("manufacturer"),
        serial_number: first("serialNumber"),
        udn: first("UDN"),
    };

    // A document with none of these is not a device description.
    (description != Description::default()).then_some(description)
}

/// Fetch and parse a description, trying each URL until one works.
///
/// Firmware revisions move this file around, so callers pass the advertised
/// `LOCATION` first and known fallback paths after it.
pub async fn fetch(client: &reqwest::Client, urls: &[String]) -> Option<Description> {
    for url in urls {
        let Ok(response) = client.get(url).send().await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = response.text().await else {
            continue;
        };
        if let Some(description) = parse(&body) {
            return Some(description);
        }
    }
    None
}

/// Build the candidate URL list: the advertised location, then `paths` on the
/// device's own address.
pub fn candidate_urls(addr: IpAddr, location: Option<&str>, paths: &[(u16, &str)]) -> Vec<String> {
    let mut urls = Vec::with_capacity(paths.len() + 1);
    if let Some(location) = location {
        urls.push(location.to_string());
    }
    for (port, path) in paths {
        urls.push(format!("http://{addr}:{port}{path}"));
    }
    urls
}

/// Convenience wrapper used by the backends.
pub async fn describe(
    client: &reqwest::Client,
    addr: IpAddr,
    location: Option<&str>,
    paths: &[(u16, &str)],
) -> Result<Description> {
    fetch(client, &candidate_urls(addr, location, paths))
        .await
        .ok_or(crate::Error::Unreachable(addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// Captured verbatim from the AVR-S720W at `:8080/description.xml`.
    const DENON: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <friendlyName>Denon AVR-S720W</friendlyName>
    <manufacturer>Denon</manufacturer>
    <modelName>*AVR-S720W</modelName>
    <modelNumber>S720W</modelNumber>
    <serialNumber>0005CD91E7A1</serialNumber>
    <UDN>uuid:5f9ec1b3-ff59-19bb-8530-0005cd91e7a1</UDN>
    <deviceList>
      <device><friendlyName>Renderer</friendlyName><modelName>Renderer</modelName></device>
    </deviceList>
  </device>
</root>"#;

    /// Captured verbatim from the UN55J6203 at `:7676/smp_3_`, in standby.
    const SAMSUNG: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:samsung.com:device:RemoteControlReceiver:1</deviceType>
    <friendlyName>[TV]Samsung LED55</friendlyName>
    <manufacturer>Samsung Electronics</manufacturer>
    <modelName>UN55J6203</modelName>
    <modelNumber>1.0</modelNumber>
    <UDN>uuid:09896800-00a0-1000-adfd-e47dbd7a6128</UDN>
  </device>
</root>"#;

    #[test]
    fn parses_the_receiver() {
        let d = parse(DENON).unwrap();
        assert_eq!(d.friendly_name.as_deref(), Some("Denon AVR-S720W"));
        assert_eq!(d.serial_number.as_deref(), Some("0005CD91E7A1"));
        assert!(d.made_by("denon"));
    }

    #[test]
    fn strips_the_model_name_marker() {
        // The receiver reports "*AVR-S720W"; the asterisk is not part of the name.
        assert_eq!(parse(DENON).unwrap().model_name.as_deref(), Some("AVR-S720W"));
    }

    #[test]
    fn prefers_the_root_device_over_nested_ones() {
        // The nested renderer must not win, or the receiver gets a useless name.
        assert_ne!(parse(DENON).unwrap().model_name.as_deref(), Some("Renderer"));
    }

    #[test]
    fn parses_the_television_in_standby() {
        // The TV closes its control ports in standby but keeps serving this,
        // which is what makes it discoverable while off.
        let d = parse(SAMSUNG).unwrap();
        assert_eq!(d.friendly_name.as_deref(), Some("[TV]Samsung LED55"));
        assert_eq!(d.model_name.as_deref(), Some("UN55J6203"));
        assert!(d.made_by("samsung"));
        assert_eq!(d.serial_number, None, "the TV publishes no serial");
    }

    #[test]
    fn uuid_strips_the_udn_prefix() {
        assert_eq!(
            parse(SAMSUNG).unwrap().uuid(),
            Some("09896800-00a0-1000-adfd-e47dbd7a6128")
        );
    }

    #[test]
    fn manufacturer_match_is_case_insensitive() {
        assert!(parse(SAMSUNG).unwrap().made_by("SAMSUNG"));
        assert!(!parse(SAMSUNG).unwrap().made_by("denon"));
    }

    #[test]
    fn rejects_documents_that_are_not_descriptions() {
        assert!(parse("<root><unclosed>").is_none());
        assert!(parse("<html><body>404</body></html>").is_none());
    }

    #[test]
    fn candidate_urls_put_the_advertised_location_first() {
        let addr = IpAddr::V4(Ipv4Addr::new(192, 168, 4, 33));
        let urls = candidate_urls(addr, Some("http://x/desc.xml"), &[(8080, "/description.xml")]);
        assert_eq!(
            urls,
            vec![
                "http://x/desc.xml".to_string(),
                "http://192.168.4.33:8080/description.xml".to_string()
            ]
        );
    }
}
