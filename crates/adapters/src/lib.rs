//! Provider integrations for the Wayfetch Downloader engine (architecture
//! brief sections 3.11 and 6.4, ADR-08).
//!
//! Each integration speaks one provider's protocol through the engine's
//! seams and keeps no state of its own: the engine owns accounts, stored
//! credentials, pending authorizations, and when to call. Requests go
//! through the host's [`wayfetch_engine::accounts::provider::ApiClient`],
//! so the network policy is enforced in one place.
//!
//! Adding a provider takes one module implementing
//! [`AccountProvider`] on [`common`], and one entry in [`INTEGRATIONS`].
//! The engine, its protocol, both hosts, and the UI need no change: the UI
//! offers sign-in for exactly the services [`providers`] returns.
//!
//! * [`common`]: endpoints, bounded requests, and answer checks.
//! * [`real_debrid`]: Real-Debrid account sign-in and status. Downloading
//!   through Real-Debrid is not implemented yet.

use std::sync::Arc;

use wayfetch_engine::accounts::provider::{AccountProvider, ApiClient};

pub mod common;
pub mod real_debrid;

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

/// Every integration, each at its official API unless `endpoints` gives
/// another for its service (for testing against a local stand-in). An
/// endpoint for a service without an integration is an error.
pub fn providers(
    client: Arc<dyn ApiClient>,
    endpoints: &[(String, String)],
) -> Result<Vec<Arc<dyn AccountProvider>>, String> {
    if let Some((service, _)) = endpoints
        .iter()
        .find(|(service, _)| !INTEGRATIONS.iter().any(|i| i.service == service))
    {
        return Err(format!("no integration for the service {service}"));
    }
    INTEGRATIONS
        .iter()
        .map(|integration| {
            let endpoint = match endpoints
                .iter()
                .rev()
                .find(|(s, _)| s == integration.service)
            {
                Some((_, url)) => Endpoint::custom(url)?,
                None => Endpoint::official(integration.official_api),
            };
            Ok((integration.build)(client.clone(), endpoint))
        })
        .collect()
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
        let unknown = providers(client, &pair("no-such-host", "https://x.example/"));
        assert!(unknown.is_err_and(|e| e.contains("no-such-host")));
    }
}
