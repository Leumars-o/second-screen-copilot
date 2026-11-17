//! mDNS advertising of the LAN server as `_ssc._tcp`. The TXT record holds only `v=1`: no key,
//! room or device name, so the advert reveals nothing beyond "an SSC PC is here".

use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::net::IpAddr;

pub const SERVICE_TYPE: &str = "_ssc._tcp.local.";

pub struct MdnsAdvert {
    daemon: ServiceDaemon,
    fullname: String,
}

impl MdnsAdvert {
    /// Register `instance._ssc._tcp.local.` on `port`, with addresses tracked automatically.
    pub fn start(instance: &str, port: u16) -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let host = format!("{instance}.local.");
        let info = ServiceInfo::new(SERVICE_TYPE, instance, &host, &[] as &[IpAddr], port, &[("v", "1")][..])?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }

    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for MdnsAdvert {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Needs multicast on a real interface, so it is not part of the gate.
    #[test]
    #[ignore = "needs a multicast-capable network interface"]
    fn advert_is_browsable() {
        let advert = MdnsAdvert::start("ssc-test-advert", 47999).unwrap();
        let browser = ServiceDaemon::new().unwrap();
        let rx = browser.browse(SERVICE_TYPE).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while std::time::Instant::now() < deadline {
            if let Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) = rx.recv_timeout(Duration::from_millis(500))
                && info.get_fullname() == advert.fullname()
            {
                assert_eq!(info.get_port(), 47999);
                found = true;
                break;
            }
        }
        let _ = browser.shutdown();
        assert!(found, "service not resolved within 5 s");
    }
}
