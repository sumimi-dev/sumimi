//! The geolocation database — downloading it.
//!
//! **The database is not shipped.** A data file of tens of megabytes does not
//! belong in a release archive, and it goes out of date every month anyway.
//! The site owner downloads it, once, with `sumimi geoip download`.
//!
//! # Which database
//!
//! **DB-IP Lite**, in the **mmdb format** — the same format MaxMind uses, and
//! the same format every other publisher uses.
//!
//! | file | what it gives | how big |
//! | --- | --- | ---: |
//! | `DBIP-Lite.mmdb` | the country, the region, the city, the coordinates | ~60 MB |
//! | `DBIP-Lite-Country.mmdb` | the country only | ~4 MB |
//!
//! The **city** one is the default: it is the one that fills the five columns
//! the schema has. The country one is for a box that cannot hold the other in
//! memory — the country is what the dashboard shows most of the time.
//!
//! # Where it goes
//!
//! Into the instance directory, under `geoip/`:
//!
//! ```text
//! sumimi/
//!   geoip/DBIP-Lite.mmdb
//! ```
//!
//! The server looks there at startup. **No database is not an error**: the
//! country then comes from the header a proxy sends, exactly as before.
//!
//! # The licence
//!
//! DB-IP Lite is under the **Creative Commons BY 4.0** licence: it can be used
//! freely, as long as the attribution is shown. The command prints it, and it
//! is in the documentation.

use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// Where the databases live.
const HOST: &str = "https://download.db-ip.com/free";

/// The attribution the licence asks for.
pub const ATTRIBUTION: &str = "Geolocation data by DB-IP (https://db-ip.com), \
under the Creative Commons BY 4.0 licence.";

/// Which database to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The country, the region, the city and the coordinates.
    City,
    /// The country only.
    Country,
}

impl Kind {
    /// The name the file has in the download URL.
    fn slug(self) -> &'static str {
        match self {
            Kind::City => "city",
            Kind::Country => "country",
        }
    }

    /// The name the file has in the instance directory.
    pub fn file_name(self) -> &'static str {
        match self {
            Kind::City => sumimi_service::geo::CITY_FILE,
            Kind::Country => sumimi_service::geo::COUNTRY_FILE,
        }
    }
}

/// Download the database into the instance directory.
///
/// The file is written to `<dir>/geoip/<name>`. The old one is kept until the
/// new one is completely there: a download that is cut in half must not leave
/// a database the server cannot read.
pub fn download(dir: &Path, kind: Kind) -> Result<()> {
    let target_dir = dir.join(sumimi_service::geo::GEOIP_DIR);
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating the directory {}", target_dir.display()))?;
    let target = target_dir.join(kind.file_name());

    // The database is published at the beginning of the month, so on the first
    // day of a new one the file may not be there yet — the month before is
    // then the newest there is.
    let mut tried = Vec::new();
    for month in months() {
        let url = format!("{HOST}/dbip-{}-lite-{month}.mmdb.gz", kind.slug());
        tried.push(url.clone());
        match fetch_and_write(&url, &target) {
            Ok(compressed) => {
                let size = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                println!("[sumimi] wrote {} ({})", target.display(), human(size));
                if let Some(size) = compressed {
                    println!("[sumimi] the download was {}", human(size));
                }
                println!(
                    "[sumimi] the server holds the file in memory: about {}",
                    human(size)
                );
                println!("[sumimi] the geolocation is ready — restart the server to use it");
                println!("[sumimi] {ATTRIBUTION}");
                return Ok(());
            }
            Err(err) if is_not_found(&err) => continue,
            Err(err) => return Err(err).with_context(|| format!("fetching {url}")),
        }
    }

    Err(anyhow!(
        "no DB-IP database could be downloaded — tried: {}",
        tried.join(", ")
    ))
}

/// The months to try, newest first.
fn months() -> Vec<String> {
    let now = chrono::Utc::now();
    vec![
        now.format("%Y-%m").to_string(),
        (now - chrono::Duration::days(31))
            .format("%Y-%m")
            .to_string(),
    ]
}

/// Whether the error is "there is no such file".
fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<ureq::Error>()
            .map(|err| matches!(err, ureq::Error::StatusCode(404)))
            .unwrap_or(false)
    })
}

/// Fetch a URL and write the file it holds, unpacked.
///
/// The old file stays until the new one is complete: a download that is cut in
/// half must not leave a database the server cannot read. The bytes go
/// through the decompressor as they arrive, so the file is never held in
/// memory.
fn fetch_and_write(url: &str, target: &Path) -> Result<Option<u64>> {
    let mut response = ureq::get(url)
        .call()
        .map_err(|err| anyhow!("fetching {url}: {err}"))?;
    let length = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    let temporary = target.with_extension("mmdb.part");
    {
        let file = std::fs::File::create(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        let mut decoder = flate2::read::GzDecoder::new(response.body_mut().as_reader());
        let mut writer = std::io::BufWriter::new(file);
        std::io::copy(&mut decoder, &mut writer)
            .with_context(|| format!("writing {}", temporary.display()))?;
        writer.flush()?;
    }
    std::fs::rename(&temporary, target)
        .with_context(|| format!("renaming {} to {}", temporary.display(), target.display()))?;
    Ok(length)
}

/// A size a human can read.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_months_are_the_newest_first() {
        let months = months();
        assert_eq!(months.len(), 2);
        assert!(
            months[0] > months[1],
            "the newest month comes first: {months:?}"
        );
        // A month is four digits, a dash and two digits.
        for month in &months {
            assert_eq!(month.len(), 7, "{month}");
            assert_eq!(&month[4..5], "-", "{month}");
        }
    }

    #[test]
    fn a_size_is_readable() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(4 * 1024 * 1024), "4.0 MB");
        assert_eq!(human(121 * 1024 * 1024), "121.0 MB");
    }

    #[test]
    fn the_file_names_are_the_ones_the_server_looks_for() {
        assert_eq!(Kind::City.file_name(), sumimi_service::geo::CITY_FILE);
        assert_eq!(Kind::Country.file_name(), sumimi_service::geo::COUNTRY_FILE);
        assert_ne!(Kind::City.file_name(), Kind::Country.file_name());
    }
}
