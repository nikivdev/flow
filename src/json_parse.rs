use anyhow::{Result, anyhow};
use serde::de::DeserializeOwned;

#[inline]
pub fn parse_json_line<T: DeserializeOwned>(line: &str) -> Result<T> {
    #[cfg(all(
        feature = "linux-host-simd-json",
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        let mut buf = line.as_bytes().to_vec();
        return simd_json::serde::from_slice(&mut buf)
            .map_err(|err| anyhow!("failed to decode json line with simd-json: {err}"));
    }

    #[cfg(not(all(
        feature = "linux-host-simd-json",
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        serde_json::from_str(line).map_err(|err| anyhow!("failed to decode json line: {err}"))
    }
}

#[inline]
pub fn parse_json_bytes_in_place<T: DeserializeOwned>(bytes: &mut [u8]) -> Result<T> {
    #[cfg(all(
        feature = "linux-host-simd-json",
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        return simd_json::serde::from_slice(bytes)
            .map_err(|err| anyhow!("failed to decode json bytes with simd-json: {err}"));
    }

    #[cfg(not(all(
        feature = "linux-host-simd-json",
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        serde_json::from_slice(bytes).map_err(|err| anyhow!("failed to decode json bytes: {err}"))
    }
}
