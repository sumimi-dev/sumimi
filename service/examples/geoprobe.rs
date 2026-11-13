//! What does the geolocation database say?
//!
//! The database is the site owner's own (`sumimi geoip download`); this asks it
//! about a few addresses, so the answer can be checked without starting the
//! server.
//!
//! ```bash
//! cargo run --release --example geoprobe -p sumimi-service -- ./sumimi/geoip/DBIP-Lite.mmdb
//! ```
use std::net::IpAddr;
use sumimi_service::geo::{MmdbResolver, Resolver};

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("the path to the .mmdb file");
    let resolver = MmdbResolver::open(std::path::Path::new(&path))?;
    for ip in [
        "8.8.8.8",
        "1.1.1.1",
        "203.0.113.7",
        "114.114.114.114",
        "2606:4700:4700::1111",
    ] {
        let address: IpAddr = ip.parse().unwrap();
        let location = resolver.lookup(address);
        println!("{ip:>24} -> {location:?}");
    }
    Ok(())
}
