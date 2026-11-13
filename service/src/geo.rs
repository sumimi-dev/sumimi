//! Where the visitor is.
//!
//! The country, the region and the city of a visit — worked out from the IP
//! address. The lookup happens in memory, on the collection path.
//!
//! There are three ways to know where a visitor is, and they are tried in this
//! order:
//!
//! 1. **The header.** A proxy in front of the server knows the country and puts
//!    it in a header (`CF-IPCountry` and friends). It is the most accurate
//!    answer, and it costs nothing.
//! 2. **The resolver.** The server's own geolocation database — an **mmdb**
//!    file, read into memory once at startup. The site owner downloads it
//!    (`sumimi geoip download`); it is not shipped, because a data file does
//!    not belong in a release archive.
//! 3. **Nothing.** The country stays empty and the report says "unknown".
//!
//! The resolver is a trait, so the server runs with or without one — and the
//! private edition, which ships its own database, uses the same path.

use std::net::IpAddr;
use std::path::Path;

use maxminddb::{geoip2, Reader};

/// What is known about where a visitor is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Location {
    /// The two-letter country code (`DE`).
    pub country: Option<String>,
    /// The region — a state, a province.
    pub region: Option<String>,
    /// The city.
    pub city: Option<String>,
    /// The latitude, in degrees.
    ///
    /// The city-level database has it; the country-level one does not.
    pub latitude: Option<f64>,
    /// The longitude, in degrees.
    pub longitude: Option<f64>,
}

/// Looks up where an IP address is.
///
/// The lookup is on the collection path, so it has to be fast: an in-memory
/// table, not a query.
pub trait Resolver: Send + Sync {
    /// Where this address is, if it is known.
    fn lookup(&self, ip: IpAddr) -> Option<Location>;
}

/// The resolver of a build that has none: everything stays unknown.
///
/// This is what the open-source build uses unless the site owner puts a
/// country in a header.
pub struct NoResolver;

impl Resolver for NoResolver {
    fn lookup(&self, _ip: IpAddr) -> Option<Location> {
        None
    }
}

/// The resolver that reads an **mmdb** database.
///
/// The file is read into memory once, at startup; the lookup is an in-memory
/// search, so it costs nothing on the collection path.
///
/// Both kinds of database are read by the same resolver: the country one
/// answers the country and the city one answers the country, the region, the
/// city and the coordinates — the decoder for the record that is not there
/// simply says "nothing", and the fields that *are* there are used.
pub struct MmdbResolver {
    reader: Reader<Vec<u8>>,
}

impl MmdbResolver {
    /// Open the database.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let reader = Reader::open_readfile(path).map_err(|err| {
            anyhow::anyhow!(
                "reading the geolocation database {}: {err}",
                path.display()
            )
        })?;
        Ok(Self { reader })
    }

    /// Whether the database says anything about this address.
    pub fn knows(&self, ip: IpAddr) -> bool {
        self.reader.lookup(ip).ok().is_some()
    }
}

impl Resolver for MmdbResolver {
    fn lookup(&self, ip: IpAddr) -> Option<Location> {
        let result = self.reader.lookup(ip).ok()?;

        // The country: the country database has it, and so has the city one.
        let country: Option<geoip2::Country<'_>> = result.decode().ok()?;
        let country = country.and_then(|c| c.country.iso_code.map(str::to_string));

        // The region, the city and the coordinates: only the city database has
        // them, so the decode fails on the country one — and that is fine.
        let record: Option<geoip2::City<'_>> = result.decode().ok().flatten();
        let region = record
            .as_ref()
            .and_then(|c| c.subdivisions.first())
            .and_then(|s| s.names.english)
            .map(str::to_string);
        let city = record
            .as_ref()
            .and_then(|c| c.city.names.english)
            .map(str::to_string);
        let latitude = record
            .as_ref()
            .and_then(|c| c.location.latitude)
            .filter(|v| v.is_finite());
        let longitude = record
            .as_ref()
            .and_then(|c| c.location.longitude)
            .filter(|v| v.is_finite());

        if country.is_none() && region.is_none() && city.is_none() {
            return None;
        }

        Some(Location {
            country,
            region,
            city,
            latitude,
            longitude,
        })
    }
}

/// The directory the geolocation databases live in, inside the instance
/// directory.
pub const GEOIP_DIR: &str = "geoip";

/// The file name of the **city** database — the country, the region, the city
/// and the coordinates.
pub const CITY_FILE: &str = "DBIP-Lite.mmdb";

/// The file name of the **country** database — the country only, and a tenth
/// of the size.
pub const COUNTRY_FILE: &str = "DBIP-Lite-Country.mmdb";

/// The environment variable that points at a database, whatever its name.
///
/// It is the way to point the server at a file the site owner put somewhere
/// else — a bigger disk, say — and the way the private edition finds its own.
pub const DATABASE_ENV: &str = "SUMIMI_GEO_DATABASE";

/// Open the resolver, if there is a database to open.
///
/// A missing database is **not** an error: the server runs without it, and the
/// country comes from the header instead. A database that is there but cannot
/// be read *is* worth a warning — the site owner asked for it and it is not
/// working.
pub fn open_if_present(path: &Path) -> Option<MmdbResolver> {
    if !path.exists() {
        return None;
    }
    match MmdbResolver::open(path) {
        Ok(resolver) => {
            tracing::info!(path = %path.display(), "the geolocation database is loaded");
            Some(resolver)
        }
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "the geolocation database cannot be read — the country comes from the header instead"
            );
            None
        }
    }
}

/// Open the resolver the environment variable points at.
pub fn from_env() -> Option<MmdbResolver> {
    let path = std::env::var(DATABASE_ENV).ok().filter(|p| !p.trim().is_empty())?;
    open_if_present(&std::path::PathBuf::from(path.trim()))
}

/// Read the country out of a header value.
///
/// The value is the country code (`DE`, `de`, `DEU`) — a proxy puts it there.
/// Anything that is not a country code is ignored: a header that says
/// `XX` or `T1` is a Tor exit node, not a place.
pub fn country_from_header(value: &str) -> Option<String> {
    let value = value.trim().to_uppercase();
    if value.len() != 2 || !value.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    // `XX` is the code for "we do not know" — a Tor exit node, a proxy that
    // does not know either. It is not a place.
    if value == "XX" {
        return None;
    }
    Some(value)
}

/// The region and the city from a header value.
///
/// Some proxies send them as `Region/City` (`Berlin/DE`) — the format the
/// header-based country lookup uses elsewhere in the industry.
pub fn region_city_from_header(value: &str) -> (Option<String>, Option<String>) {
    let value = value.trim();
    if value.is_empty() {
        return (None, None);
    }
    match value.split_once('/') {
        Some((region, city)) => (
            non_empty(region),
            non_empty(city),
        ),
        None => (None, non_empty(value)),
    }
}

/// A non-empty string, or nothing.
fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_country_code_is_read_from_the_header() {
        assert_eq!(country_from_header("DE"), Some("DE".to_string()));
        assert_eq!(country_from_header("de"), Some("DE".to_string()));
        assert_eq!(country_from_header(" de "), Some("DE".to_string()));
    }

    #[test]
    fn something_that_is_not_a_country_is_ignored() {
        // A Tor exit node, a list, an empty value.
        assert_eq!(country_from_header("XX"), None);
        assert_eq!(country_from_header("T1"), None);
        assert_eq!(country_from_header(""), None);
        assert_eq!(country_from_header("DEU"), None);
        assert_eq!(country_from_header("1A"), None);
    }

    #[test]
    fn a_file_that_is_not_a_database_is_refused() {
        let path = std::env::temp_dir().join(format!("sumimi-not-a-database-{}.mmdb", std::process::id()));
        std::fs::write(&path, b"this is not a database").unwrap();
        assert!(MmdbResolver::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_database_that_is_not_there_is_not_an_error() {
        let path = std::env::temp_dir().join(format!("sumimi-no-database-{}.mmdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(open_if_present(&path).is_none());
    }

    #[test]
    fn a_region_and_a_city_are_read_from_the_header() {
        assert_eq!(
            region_city_from_header("Berlin/DE"),
            (Some("Berlin".to_string()), Some("DE".to_string()))
        );
        assert_eq!(
            region_city_from_header("Berlin"),
            (None, Some("Berlin".to_string()))
        );
        assert_eq!(region_city_from_header(""), (None, None));
    }
}
