//! Provider integrations and helper-program plugins for the engine (ADR-08).
//!
//! Integrations keep no state of their own and reach the network only
//! through the engine's [`wayfetch_engine::accounts::provider::ApiClient`].
//! Adding a provider takes one module implementing [`AccountProvider`] and
//! one entry in [`INTEGRATIONS`]; the UI offers whatever [`providers`]
//! returns. A file host implements [`FileHost`] and is listed in
//! [`FILE_HOSTS`].

use std::sync::Arc;

use wayfetch_engine::accounts::provider::{AccountProvider, ApiClient};
use wayfetch_engine::hosting::FileHost;

pub mod common;
pub mod confine;
pub mod ffmpeg;
pub mod mega;
pub mod plugin;
pub mod plugins;
pub mod real_debrid;
pub mod release;
pub mod sevenzip;
pub mod updates;
pub mod youtube;
pub mod ytdlp;

pub use common::Endpoint;

/// One integration a host can register.
pub struct Integration {
    /// The service's ID, as in the UI's catalogue.
    pub service: &'static str,
    /// The provider's own API.
    pub official_api: &'static str,
    /// Builds the integration on `client`, at an endpoint.
    pub build: fn(Arc<dyn ApiClient>, Endpoint) -> Arc<dyn AccountProvider>,
}

/// Every integration this build ships.
pub const INTEGRATIONS: &[Integration] = &[Integration {
    service: real_debrid::SERVICE,
    official_api: real_debrid::OFFICIAL_API,
    build: real_debrid::RealDebrid::provider,
}];

/// One file host a host can register.
pub struct HostIntegration {
    /// The service's ID, as in the UI's catalogue.
    pub service: &'static str,
    /// The host's own API.
    pub official_api: &'static str,
    /// Builds the integration on `client`, at an endpoint.
    pub build: fn(Arc<dyn ApiClient>, Endpoint) -> Arc<dyn FileHost>,
}

/// Every file host this build ships.
pub const FILE_HOSTS: &[HostIntegration] = &[HostIntegration {
    service: mega::SERVICE,
    official_api: mega::OFFICIAL_API,
    build: mega::Mega::host,
}];

/// Every integration, each at its official API unless `endpoints` gives
/// another for its service (for testing against a local stand-in). An
/// endpoint for a service without an integration or file host is an error.
pub fn providers(
    client: Arc<dyn ApiClient>,
    endpoints: &[(String, String)],
) -> Result<Vec<Arc<dyn AccountProvider>>, String> {
    if let Some((service, _)) = endpoints.iter().find(|(service, _)| {
        !INTEGRATIONS.iter().any(|i| i.service == service)
            && !FILE_HOSTS.iter().any(|h| h.service == service)
    }) {
        return Err(format!("no integration for the service {service}"));
    }
    INTEGRATIONS
        .iter()
        .map(|integration| {
            let endpoint = endpoint(endpoints, integration.service, integration.official_api)?;
            Ok((integration.build)(client.clone(), endpoint))
        })
        .collect()
}

/// Every file host, each at its official API unless `endpoints` gives
/// another for its service, as for [`providers`].
pub fn file_hosts(
    client: Arc<dyn ApiClient>,
    endpoints: &[(String, String)],
) -> Result<Vec<Arc<dyn FileHost>>, String> {
    FILE_HOSTS
        .iter()
        .map(|host| {
            let endpoint = endpoint(endpoints, host.service, host.official_api)?;
            Ok((host.build)(client.clone(), endpoint))
        })
        .collect()
}

fn endpoint(
    endpoints: &[(String, String)],
    service: &str,
    official: &str,
) -> Result<Endpoint, String> {
    match endpoints.iter().rev().find(|(s, _)| s == service) {
        Some((_, url)) => Endpoint::custom(url),
        None => Ok(Endpoint::official(official)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayfetch_engine::accounts::provider::{ApiError, ApiRequest, ApiResponse};
    use wayfetch_engine::transfer::writer::BoxFuture;

    struct Unused;

    impl ApiClient for Unused {
        fn send<'a>(&'a self, _: &'a ApiRequest) -> BoxFuture<'a, Result<ApiResponse, ApiError>> {
            unreachable!("no request is sent")
        }
    }

    #[test]
    fn every_integration_is_registered_once_under_its_service() {
        let all = providers(Arc::new(Unused), &[]).unwrap();
        assert_eq!(all.len(), INTEGRATIONS.len());
        for (provider, integration) in all.iter().zip(INTEGRATIONS) {
            assert_eq!(provider.service().as_str(), integration.service);
            assert_eq!(provider.support().service.as_str(), integration.service);
        }
        let mut services: Vec<_> = INTEGRATIONS.iter().map(|i| i.service).collect();
        services.sort_unstable();
        services.dedup();
        assert_eq!(services.len(), INTEGRATIONS.len());
    }

    #[test]
    fn endpoints_must_name_an_integration_and_be_safe() {
        let client: Arc<dyn ApiClient> = Arc::new(Unused);
        let pair = |s: &str, u: &str| [(s.to_owned(), u.to_owned())];
        assert!(providers(client.clone(), &pair("real-debrid", "http://127.0.0.1:9/")).is_ok());
        assert!(providers(client.clone(), &pair("real-debrid", "http://rd.example/")).is_err());
        assert!(providers(client.clone(), &pair("mega", "http://127.0.0.1:9/")).is_ok());
        assert!(file_hosts(client.clone(), &pair("mega", "http://mega.example/")).is_err());
        let unknown = providers(client, &pair("no-such-host", "https://x.example/"));
        assert!(unknown.is_err_and(|e| e.contains("no-such-host")));
    }
}
