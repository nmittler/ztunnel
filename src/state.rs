// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::identity::SecretManager;
use crate::proxy;
use crate::proxy::{Error, OnDemandDnsLabels};
use crate::state::policy::PolicyStore;
use crate::state::service::ServiceStore;
use crate::state::service::{Service, ServiceDescription};
use crate::state::workload::{
    address::Address, gatewayaddress::Destination, network_addr, NamespacedHostname,
    NetworkAddress, Protocol, WaypointError, Workload, WorkloadStore,
};
use crate::tls;
use crate::xds::istio::security::Authorization as XdsAuthorization;
use crate::xds::istio::workload::Address as XdsAddress;
use crate::xds::metrics::Metrics;
use crate::xds::{AdsClient, Demander, LocalClient, ProxyStateUpdater};
use crate::{cert_fetcher, config, rbac, readiness, xds};
use rand::prelude::IteratorRandom;
use rand::seq::SliceRandom;
use std::collections::{HashMap, HashSet};
use std::convert::Into;
use std::default::Default;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::sync::Notify;
use tracing::{debug, trace, warn};

use trust_dns_resolver::config::*;
use trust_dns_resolver::{TokioAsyncResolver, TokioHandle};

pub mod policy;
pub mod service;
pub mod workload;

#[derive(Debug, Eq, PartialEq, Clone, serde::Serialize)]
pub struct Upstream {
    pub workload: Workload,
    pub port: u16,
    pub sans: Vec<String>,
    pub destination_service: Option<ServiceDescription>,
}

impl fmt::Display for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Upstream{{{} with uid {}:{} via {} ({:?}) sans:{:?}}}",
            self.workload.name,
            self.workload.uid,
            self.port,
            self.workload
                .gateway_address
                .map(|x| format!("{x}"))
                .unwrap_or_else(|| "None".into()),
            self.workload.protocol,
            self.sans,
        )
    }
}

/// The current state information for this proxy.
#[derive(serde::Serialize, Default, Debug)]
pub struct ProxyState {
    #[serde(flatten)]
    pub workloads: WorkloadStore,

    #[serde(flatten)]
    pub services: ServiceStore,

    #[serde(flatten)]
    pub policies: PolicyStore,
}

impl ProxyState {
    /// Find either a workload or service by the destination.
    pub fn find_destination(&self, dest: &Destination) -> Option<Address> {
        match dest {
            Destination::Address(addr) => self.find_address(addr),
            Destination::Hostname(hostname) => self.find_hostname(hostname),
        }
    }

    /// Find either a workload or a service by address.
    pub fn find_address(&self, network_addr: &NetworkAddress) -> Option<Address> {
        // 1. handle workload ip, if workload not found fallback to service.
        match self.workloads.find_address(network_addr) {
            None => {
                // 2. handle service
                if let Some(svc) = self.services.get_by_vip(network_addr) {
                    return Some(Address::Service(Box::new(svc)));
                }
                None
            }
            Some(wl) => Some(Address::Workload(Box::new(wl))),
        }
    }

    /// Find either a workload or a service by hostname.
    pub fn find_hostname(&self, name: &NamespacedHostname) -> Option<Address> {
        // Hostnames for services are more common, so lookup service first and fallback
        // to workload.
        match self.services.get_by_namespaced_host(name) {
            None => {
                // Workload hostnames are globally unique, so ignore the namespace.
                self.workloads
                    .find_hostname(&name.hostname)
                    .map(|wl| Address::Workload(Box::new(wl)))
            }
            Some(svc) => Some(Address::Service(Box::new(svc))),
        }
    }

    pub fn find_upstream(&self, network: &str, addr: SocketAddr) -> Option<Upstream> {
        if let Some(svc) = self.services.get_by_vip(&network_addr(network, addr.ip())) {
            let Some(target_port) = svc.ports.get(&addr.port()) else {
                debug!(
                    "found VIP {}, but port {} was unknown",
                    addr.ip(),
                    addr.port()
                );
                return None;
            };
            // Randomly pick an upstream
            // TODO: do this more efficiently, and not just randomly
            let Some((_, ep)) = svc.endpoints.iter().choose(&mut rand::thread_rng()) else {
                debug!("VIP {} has no healthy endpoints", addr);
                return None;
            };
            let Some(wl) = self.workloads.find_uid(&ep.workload_uid) else {
                debug!("failed to fetch workload for {}", ep.workload_uid);
                return None;
            };
            // If endpoint overrides the target port, use that instead
            let target_port = ep.port.get(&addr.port()).unwrap_or(target_port);
            let us = Upstream {
                workload: wl,
                port: *target_port,
                sans: svc.subject_alt_names.clone(),
                destination_service: Some((&svc).into()),
            };
            return Some(us);
        }
        if let Some(wl) = self
            .workloads
            .find_address(&network_addr(network, addr.ip()))
        {
            let us = Upstream {
                workload: wl,
                port: addr.port(),
                sans: Vec::new(),
                destination_service: None,
            };
            return Some(us);
        }
        None
    }
}

/// Wrapper around [ProxyState] that provides additional methods for requesting information
/// on-demand.
#[derive(serde::Serialize, Debug, Clone)]
pub struct DemandProxyState {
    #[serde(flatten)]
    pub state: Arc<RwLock<ProxyState>>,

    /// If present, used to request on-demand updates for workloads.
    #[serde(skip_serializing)]
    demand: Option<Demander>,

    #[serde(skip_serializing)]
    dns_resolver: DnsResolver,
}

impl DemandProxyState {
    pub fn new(
        state: Arc<RwLock<ProxyState>>,
        demand: Option<Demander>,
        dns_resolver_cfg: ResolverConfig,
        dns_resolver_opts: ResolverOpts,
    ) -> Self {
        let dns_resolver = DnsResolver::new(dns_resolver_cfg, dns_resolver_opts);
        Self {
            state,
            demand,
            dns_resolver,
        }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, ProxyState> {
        self.state.read().unwrap()
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, ProxyState> {
        self.state.write().unwrap()
    }

    pub async fn assert_rbac(&self, conn: &rbac::Connection) -> bool {
        let nw_addr = network_addr(&conn.dst_network, conn.dst.ip());
        let Some(wl) = self.fetch_workload(&nw_addr).await else {
            debug!("destination workload not found {}", nw_addr);
            return false;
        };

        let state = self.state.read().unwrap();

        // We can get policies from namespace, global, and workload...
        let ns = state.policies.get_by_namespace(&wl.namespace);
        let global = state.policies.get_by_namespace("");
        let workload = wl.authorization_policies.iter();

        // Aggregate all of them based on type
        let (allow, deny): (Vec<_>, Vec<_>) = ns
            .iter()
            .chain(global.iter())
            .chain(workload)
            .filter_map(|k| state.policies.get(k))
            .partition(|p| p.action == rbac::RbacAction::Allow);

        trace!(
            allow = allow.len(),
            deny = deny.len(),
            "checking connection"
        );

        // Allow and deny logic follows https://istio.io/latest/docs/reference/config/security/authorization-policy/

        // "If there are any DENY policies that match the request, deny the request."
        for pol in deny.iter() {
            if pol.matches(conn) {
                debug!(policy = pol.to_key(), "deny policy match");
                return false;
            } else {
                trace!(policy = pol.to_key(), "deny policy does not match");
            }
        }
        // "If there are no ALLOW policies for the workload, allow the request."
        if allow.is_empty() {
            debug!("no allow policies, allow");
            return true;
        }
        // "If any of the ALLOW policies match the request, allow the request."
        for pol in allow.iter() {
            if pol.matches(conn) {
                debug!(policy = pol.to_key(), "allow policy match");
                return true;
            } else {
                trace!(policy = pol.to_key(), "allow policy does not match");
            }
        }
        // "Deny the request."
        debug!("no allow policies matched");
        false
    }

    // this should only be called once per request (for the workload itself and potentially its waypoint)
    pub async fn load_balance(
        &self,
        dst_workload: &Workload,
        src_workload: &Workload,
        metrics: Arc<proxy::Metrics>,
    ) -> Result<IpAddr, Error> {
        // TODO: add more sophisticated routing logic, perhaps based on ipv4/ipv6 support underneath us.
        // if/when we support that, this function may need to move to get access to the necessary metadata.
        // Randomly pick an IP
        // TODO: do this more efficiently, and not just randomly
        if let Some(ip) = dst_workload.workload_ips.choose(&mut rand::thread_rng()) {
            return Ok(*ip);
        }
        if dst_workload.hostname.is_empty() {
            debug!(
                "workload {} has no suitable workload IPs for routing",
                dst_workload.name
            );
            return Err(Error::NoValidDestination(Box::new(dst_workload.clone())));
        }

        // Resolve the destination workload to a set of IPs.
        match self
            .dns_resolver
            .resolve_host(dst_workload, src_workload, metrics)
            .await
        {
            Some(rdns) => {
                // TODO: add more sophisticated routing logic, perhaps based on ipv4/ipv6 support underneath us.
                // if/when we support that, this function may need to move to get access to the necessary metadata.
                // Randomly pick an IP
                // TODO: do this more efficiently, and not just randomly
                let Some(ip) = rdns.ips.iter().choose(&mut rand::thread_rng()) else {
                    return Err(Error::EmptyResolvedAddresses(dst_workload.uid.clone()));
                };
                Ok(*ip)
            }
            None => Err(Error::NoResolvedAddresses(dst_workload.uid.clone())),
        }
    }

    pub async fn fetch_workload_services(
        &self,
        addr: &NetworkAddress,
    ) -> Option<(Workload, Vec<Service>)> {
        // Wait for it on-demand, *if* needed
        debug!(%addr, "fetch workload and service");
        let fetch = |addr: &NetworkAddress| {
            let state = self.state.read().unwrap();
            state.workloads.find_address(addr).map(|wl| {
                let svc = state.services.get_by_workload(&wl);
                (wl, svc)
            })
        };
        if let Some(wl) = fetch(addr) {
            return Some(wl);
        }
        self.fetch_on_demand(addr.to_string()).await;
        fetch(addr)
    }

    // only support workload
    pub async fn fetch_workload(&self, addr: &NetworkAddress) -> Option<Workload> {
        // Wait for it on-demand, *if* needed
        debug!(%addr, "fetch workload");
        if let Some(wl) = self.state.read().unwrap().workloads.find_address(addr) {
            return Some(wl);
        }
        self.fetch_on_demand(addr.to_string()).await;
        self.state.read().unwrap().workloads.find_address(addr)
    }

    // only support workload
    pub async fn fetch_workload_by_uid(&self, uid: &str) -> Option<Workload> {
        // Wait for it on-demand, *if* needed
        debug!(%uid, "fetch workload");
        if let Some(wl) = self.state.read().unwrap().workloads.find_uid(uid) {
            return Some(wl);
        }
        self.fetch_on_demand(uid.to_string()).await;
        self.state.read().unwrap().workloads.find_uid(uid)
    }

    pub async fn fetch_upstream(&self, network: &str, addr: SocketAddr) -> Option<Upstream> {
        self.fetch_address(&network_addr(network, addr.ip())).await;
        self.state.read().unwrap().find_upstream(network, addr)
    }

    pub async fn fetch_waypoint(
        &self,
        wl: &Workload,
        workload_ip: IpAddr,
    ) -> Result<Option<Upstream>, WaypointError> {
        let Some(gw_address) = &wl.waypoint else {
            return Ok(None);
        };
        // Even in this case, we are picking a single upstream pod and deciding if it has a remote proxy.
        // Typically this is all or nothing, but if not we should probably send to remote proxy if *any* upstream has one.
        let wp_nw_addr = match &gw_address.destination {
            Destination::Address(ip) => ip,
            Destination::Hostname(_) => {
                return Err(WaypointError::UnsupportedFeature(
                    "hostname lookup not supported yet".to_string(),
                ));
            }
        };
        let wp_socket_addr = SocketAddr::new(wp_nw_addr.address, gw_address.hbone_mtls_port);
        match self
            .fetch_upstream(&wp_nw_addr.network, wp_socket_addr)
            .await
        {
            Some(mut upstream) => {
                debug!(%wl.name, "found waypoint upstream");
                match set_gateway_address(&mut upstream, workload_ip, gw_address.hbone_mtls_port) {
                    Ok(_) => Ok(Some(upstream)),
                    Err(e) => {
                        debug!(%wl.name, "failed to set gateway address for upstream: {}", e);
                        Err(WaypointError::FindWaypointError(wl.name.to_owned()))
                    }
                }
            }
            None => {
                debug!(%wl.name, "waypoint upstream not found");
                Err(WaypointError::FindWaypointError(wl.name.to_owned()))
            }
        }
    }

    /// Looks for either a workload or service by the destination. If not found locally,
    /// attempts to fetch on-demand.
    pub async fn fetch_destination(&self, dest: &Destination) -> Option<Address> {
        match dest {
            Destination::Address(addr) => self.fetch_address(addr).await,
            Destination::Hostname(hostname) => self.fetch_hostname(hostname).await,
        }
    }

    /// Looks for the given address to find either a workload or service by IP. If not found
    /// locally, attempts to fetch on-demand.
    pub async fn fetch_address(&self, network_addr: &NetworkAddress) -> Option<Address> {
        // Wait for it on-demand, *if* needed
        debug!(%network_addr.address, "fetch address");
        if let Some(address) = self.state.read().unwrap().find_address(network_addr) {
            return Some(address);
        }
        // if both cache not found, start on demand fetch
        self.fetch_on_demand(network_addr.to_string()).await;
        self.state.read().unwrap().find_address(network_addr)
    }

    /// Looks for the given hostname to find either a workload or service by IP. If not found
    /// locally, attempts to fetch on-demand.
    pub async fn fetch_hostname(&self, hostname: &NamespacedHostname) -> Option<Address> {
        // Wait for it on-demand, *if* needed
        debug!(%hostname, "fetch hostname");
        if let Some(address) = self.state.read().unwrap().find_hostname(hostname) {
            return Some(address);
        }
        // if both cache not found, start on demand fetch
        self.fetch_on_demand(hostname.to_string()).await;
        self.state.read().unwrap().find_hostname(hostname)
    }

    async fn fetch_on_demand(&self, key: String) {
        if let Some(demand) = &self.demand {
            debug!(%key, "sending demand request");
            demand
                .demand(xds::ADDRESS_TYPE.to_string(), key.clone())
                .await
                .recv()
                .await;
            debug!(%key, "on demand ready");
        }
    }
}

/// A Dns Resolver is responsible for the DNS resolving task for given hostnames
#[derive(Default, Debug, Clone)]
struct DnsResolver {
    // Map of resolved hostnames.
    resolved: Arc<RwLock<HashMap<String, ResolvedDns>>>,

    // Map of in-progress resolution requests.
    in_progress: Arc<Mutex<HashMap<String, Arc<Notify>>>>,

    dns_resolver_cfg: ResolverConfig,

    dns_resolver_opts: ResolverOpts,
}

#[derive(serde::Serialize, Default, Debug, Clone)]
struct ResolvedDns {
    hostname: String,
    ips: HashSet<IpAddr>,
    #[serde(skip_serializing)]
    initial_query: Option<std::time::Instant>,
    // the shortest DNS ttl of all records in the response; used for cache refresh.
    // we use the shortest ttl rather than just relying on the older records so we don't
    // load-balance to just the older records as the records with early ttl expire.
    dns_refresh_rate: std::time::Duration,
}

impl DnsResolver {
    fn new(dns_resolver_cfg: ResolverConfig, dns_resolver_opts: ResolverOpts) -> Self {
        Self {
            resolved: Arc::new(RwLock::new(HashMap::new())),
            in_progress: Arc::new(Mutex::new(HashMap::new())),
            dns_resolver_cfg,
            dns_resolver_opts,
        }
    }

    async fn resolve_host(
        &self,
        workload: &Workload,
        src_workload: &Workload,
        metrics: Arc<proxy::Metrics>,
    ) -> Option<ResolvedDns> {
        let labels = OnDemandDnsLabels::new()
            .with_destination(workload)
            .with_source(src_workload);
        metrics.as_ref().on_demand_dns.get_or_create(&labels).inc();

        // First, check if we've already resolved this host.
        let hostname = workload.hostname.to_owned();
        if let Some(resolved) = self._find_resolved_host(&hostname) {
            return Some(resolved);
        }

        metrics
            .as_ref()
            .on_demand_dns_cache_misses
            .get_or_create(&labels)
            .inc();

        // We need to resolve the host. The first request here will create the
        // notify and will perform the resolution. Requests that follow will
        // just wait for the results of the first request.
        let (n, is_first) = self._get_or_create_notify(&hostname);
        if is_first {
            // We're the first: perform the resolution of the host.
            self._resolve_host(workload).await;

            // notify all waiters after the dns resolving task completed
            n.notify_waiters();

            // All threads that were waiting have been notified and the
            // local cache has been updated. We can now go ahead and
            // remove the in-progress notify object.
            self.in_progress.lock().unwrap().remove(hostname.as_str());
        } else {
            // Wait for the in-progress resolution to complete.
            n.notified().await;
        }

        // At this point, resolution has completed. Just serve from local
        // cache.
        self._find_resolved_host(&hostname)
    }

    fn _get_or_create_notify(&self, hostname: &String) -> (Arc<Notify>, bool) {
        let mut in_progress = self.in_progress.lock().unwrap();
        match in_progress.get(hostname) {
            Some(n) => (n.clone(), false),
            None => {
                let n = Arc::new(Notify::new());
                in_progress.insert(hostname.clone(), n.clone());
                (n, true)
            }
        }
    }

    fn _find_resolved_host(&self, hostname: &String) -> Option<ResolvedDns> {
        self.resolved
            .read()
            .unwrap()
            .get(hostname)
            .filter(|rdns| {
                rdns.initial_query.is_some()
                    && rdns.initial_query.unwrap().elapsed() < rdns.dns_refresh_rate
            })
            .cloned()
    }

    async fn _resolve_host(&self, workload: &Workload) {
        let workload_uid = workload.uid.to_owned();
        let hostname = workload.hostname.to_owned();
        trace!("dns workload async task started for {:?}", &hostname);

        let resolver_result = TokioAsyncResolver::new(
            self.dns_resolver_cfg.to_owned(),
            self.dns_resolver_opts,
            TokioHandle,
        );
        if resolver_result.is_err() {
            warn!(
                "system dns async resolution: error creating resolver for workload {} is: {:?}",
                &workload_uid, resolver_result
            );
            return;
        }
        let r = resolver_result.unwrap();

        let resp = r.lookup_ip(&hostname).await;
        if resp.is_err() {
            warn!(
                "system dns async resolution: error response for workload {} is: {:?}",
                &workload_uid, resp
            );
            return;
        } else {
            trace!(
                "system dns async resolution: response for workload {} is: {:?}",
                &workload_uid,
                resp
            );
        }
        let resp = resp.unwrap();
        let mut dns_refresh_rate = std::time::Duration::from_secs(u64::MAX);
        let ips = HashSet::from_iter(resp.as_lookup().record_iter().filter_map(|record| {
            if record.rr_type().is_ip_addr() {
                let record_ttl = u64::from(record.ttl());
                if let Some(ipv4) = record.data().unwrap().as_a() {
                    if record_ttl < dns_refresh_rate.as_secs() {
                        dns_refresh_rate = std::time::Duration::from_secs(record_ttl);
                    }
                    return Some(IpAddr::V4(*ipv4));
                }
                if let Some(ipv6) = record.data().unwrap().as_aaaa() {
                    if record_ttl < dns_refresh_rate.as_secs() {
                        dns_refresh_rate = std::time::Duration::from_secs(record_ttl);
                    }
                    return Some(IpAddr::V6(*ipv6));
                }
                return None;
            }
            None
        }));
        if ips.is_empty() {
            // if we have no DNS records with a TTL to lean on; lets try to refresh again in 60s
            dns_refresh_rate = std::time::Duration::from_secs(60);
        }
        let now = std::time::Instant::now();
        let rdns = ResolvedDns {
            hostname: hostname.to_owned(),
            ips,
            initial_query: Some(now),
            dns_refresh_rate,
        };
        self.resolved.write().unwrap().insert(hostname, rdns);
    }
}

pub fn set_gateway_address(
    us: &mut Upstream,
    workload_ip: IpAddr,
    hbone_port: u16,
) -> anyhow::Result<()> {
    if us.workload.gateway_address.is_none() {
        us.workload.gateway_address = Some(match us.workload.protocol {
            Protocol::HBONE => {
                let ip = us
                    .workload
                    .waypoint_svc_ip_address()?
                    .unwrap_or(workload_ip);
                SocketAddr::from((ip, hbone_port))
            }
            Protocol::TCP => SocketAddr::from((workload_ip, us.port)),
        });
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct ProxyStateManager {
    #[serde(flatten)]
    state: DemandProxyState,

    #[serde(skip_serializing)]
    xds_client: Option<AdsClient>,
}

impl ProxyStateManager {
    pub async fn new(
        config: config::Config,
        metrics: Metrics,
        awaiting_ready: readiness::BlockReady,
        cert_manager: Arc<SecretManager>,
    ) -> anyhow::Result<ProxyStateManager> {
        let cert_fetcher = cert_fetcher::new(&config, cert_manager);
        let state: Arc<RwLock<ProxyState>> = Arc::new(RwLock::new(ProxyState::default()));
        let xds_client = if config.xds_address.is_some() {
            let updater = ProxyStateUpdater::new(state.clone(), cert_fetcher.clone());
            let tls_client_fetcher = Box::new(tls::FileClientCertProviderImpl::RootCert(
                config.xds_root_cert.clone(),
            ));
            Some(
                xds::Config::new(config.clone(), tls_client_fetcher)
                    .with_watched_handler::<XdsAddress>(xds::ADDRESS_TYPE, updater.clone())
                    .with_watched_handler::<XdsAuthorization>(xds::AUTHORIZATION_TYPE, updater)
                    .build(metrics, awaiting_ready),
            )
        } else {
            None
        };
        if let Some(cfg) = config.local_xds_config {
            let local_client = LocalClient {
                cfg,
                state: state.clone(),
                cert_fetcher,
            };
            local_client.run().await?;
        }
        let demand = xds_client.as_ref().and_then(AdsClient::demander);
        let dns_resolver = DnsResolver::new(config.dns_resolver_cfg, config.dns_resolver_opts);
        Ok(ProxyStateManager {
            xds_client,
            state: DemandProxyState {
                state,
                demand,
                dns_resolver,
            },
        })
    }

    pub fn state(&self) -> DemandProxyState {
        self.state.clone()
    }

    pub async fn run(self) -> anyhow::Result<()> {
        match self.xds_client {
            Some(xds) => xds.run().await.map_err(|e| anyhow::anyhow!(e)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, time::Duration};

    use super::*;
    use crate::test_helpers;

    #[tokio::test]
    async fn lookup_address() {
        let mut state = ProxyState::default();
        state
            .workloads
            .insert(test_helpers::test_default_workload())
            .unwrap();
        state.services.insert(test_helpers::mock_default_service());

        let mock_proxy_state = DemandProxyState::new(
            Arc::new(RwLock::new(state)),
            None,
            ResolverConfig::default(),
            ResolverOpts::default(),
        );

        // Some from Address
        let dst = Destination::Address(NetworkAddress {
            network: "".to_string(),
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
        });
        test_helpers::assert_eventually(
            Duration::from_secs(5),
            || mock_proxy_state.fetch_destination(&dst),
            Some(Address::Workload(Box::new(
                test_helpers::test_default_workload(),
            ))),
        )
        .await;

        // Some from Hostname
        let dst = Destination::Hostname(NamespacedHostname {
            namespace: "default".to_string(),
            hostname: "defaulthost".to_string(),
        });
        test_helpers::assert_eventually(
            Duration::from_secs(5),
            || mock_proxy_state.fetch_destination(&dst),
            Some(Address::Service(Box::new(
                test_helpers::mock_default_service(),
            ))),
        )
        .await;

        // None from Address
        let dst = Destination::Address(NetworkAddress {
            network: "".to_string(),
            address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
        });
        test_helpers::assert_eventually(
            Duration::from_secs(5),
            || mock_proxy_state.fetch_destination(&dst),
            None,
        )
        .await;

        // None from Hostname
        let dst = Destination::Hostname(NamespacedHostname {
            namespace: "default".to_string(),
            hostname: "nothost".to_string(),
        });
        test_helpers::assert_eventually(
            Duration::from_secs(5),
            || mock_proxy_state.fetch_destination(&dst),
            None,
        )
        .await;
    }
}
