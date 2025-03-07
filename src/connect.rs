use std::{convert::Infallible, ops::Deref, sync::Arc, time::Duration};

use async_compat::Compat;

use china::test_china;
use futures_util::future::select_all;

use once_cell::sync::Lazy;

use parking_lot::RwLock;
use rand::Rng;
use smol::{prelude::*, Task};
use smol_timeout::TimeoutExt;

use crate::{
    config::{get_conninfo_store, ConnectOpt, Opt, CONFIG},
    connect::tunnel::{BinderTunnelParams, ClientTunnel, EndpointSource, TunnelStatus},
    conninfo_store::ConnInfoStore,
};

use crate::china;

mod dns;
mod port_forwarder;
mod socks5;
mod stats;
mod tunnel;
pub(crate) mod vpn;

/// Main function for `connect` subcommand
pub fn start_main_connect() {
    Lazy::force(&CONNECT_TASK);
}

static METRIC_SESSION_ID: Lazy<i64> = Lazy::new(|| {
    let mut rng = rand::thread_rng();
    rng.gen()
});

/// The configured binder client
static CONNINFO_STORE: Lazy<Arc<ConnInfoStore>> = Lazy::new(|| {
    Arc::new({
        let (common, auth, exit_host) = match CONFIG.deref() {
            Opt::Connect(c) => (
                &c.common,
                &c.auth,
                c.exit_server.clone().unwrap_or_default(),
            ),
            _ => panic!(),
        };
        log::debug!("about to construct the global conninfo");
        smol::future::block_on(async move {
            loop {
                log::debug!("inside the blocked-on future for conninfo");
                match get_conninfo_store(common, auth, &exit_host).await {
                    Ok(val) => return val,
                    Err(err) => log::warn!("could not get conninfo store: {:?}", err),
                }
                smol::Timer::after(Duration::from_secs(1)).await;
            }
        })
    })
});

static CONNECT_CONFIG: Lazy<ConnectOpt> = Lazy::new(|| match CONFIG.deref() {
    Opt::Connect(c) => c.clone(),
    _ => panic!(),
});

static SHOULD_USE_BRIDGES: Lazy<bool> = Lazy::new(|| {
    smol::future::block_on(async {
        // Test china
        let is_china = test_china().timeout(Duration::from_secs(2)).await;
        match is_china {
            Some(Err(_)) | None => {
                log::warn!(
                    "could not tell whether or not we're in China , so assuming that we are!",
                );
                true
            }
            Some(Ok(true)) => {
                log::info!("we are in CHINA :O");
                true
            }
            _ => {
                log::info!("not in China :)");
                CONNECT_CONFIG.use_bridges
            }
        }
    })
});

type StatusCallback = Box<dyn Fn(TunnelStatus) + Send + Sync + 'static>;
static TUNNEL_STATUS_CALLBACK: Lazy<RwLock<StatusCallback>> = Lazy::new(|| {
    RwLock::new(Box::new(|addr| {
        log::debug!("tunnel reported {:?} to dummy", addr);
    }))
});

pub static TUNNEL: Lazy<ClientTunnel> = Lazy::new(|| {
    let endpoint = {
        if let Some(override_url) = CONNECT_CONFIG.override_connect.clone() {
            EndpointSource::Independent {
                endpoint: override_url,
            }
        } else {
            EndpointSource::Binder(BinderTunnelParams {
                cstore: CONNINFO_STORE.clone(),
                exit_server: CONNECT_CONFIG.exit_server.clone(),
                use_bridges: *SHOULD_USE_BRIDGES,
                force_bridge: CONNECT_CONFIG.force_bridge,
                force_protocol: CONNECT_CONFIG.force_protocol.clone(),
            })
        }
    };
    log::debug!("gonna construct the tunnel");
    ClientTunnel::new(endpoint, |status| TUNNEL_STATUS_CALLBACK.read()(status))
});

static CONNECT_TASK: Lazy<Task<Infallible>> = Lazy::new(|| {
    smolscale::spawn(async {
        // print out config file
        log::info!(
            "connect mode starting: exit = {:?}, force_protocol = {:?}, use_bridges = {}",
            CONNECT_CONFIG.exit_server,
            CONNECT_CONFIG.force_protocol,
            CONNECT_CONFIG.use_bridges
        );

        // http proxy
        let _socks2h = smolscale::spawn(Compat::new(crate::socks2http::run_tokio(
            CONNECT_CONFIG.http_listen,
            {
                let mut addr = CONNECT_CONFIG.socks5_listen;
                addr.set_ip("127.0.0.1".parse().unwrap());
                addr
            },
        )));

        // socks5 proxy
        let socks5_fut = smolscale::spawn(socks5::socks5_loop(
            CONNECT_CONFIG.socks5_listen,
            CONNECT_CONFIG.exclude_prc,
        ));
        // dns
        let dns_fut = smolscale::spawn(dns::dns_loop(CONNECT_CONFIG.dns_listen));
        // refresh
        let refresh_fut = smolscale::spawn(async {
            loop {
                if let Err(err) = CONNINFO_STORE.refresh().await {
                    log::warn!("error refreshing store: {:?}", err);
                }
                smol::Timer::after(Duration::from_secs(120)).await;
            }
        });

        // port forwarders
        let port_forwarders: Vec<_> = CONNECT_CONFIG
            .forward_ports
            .iter()
            .map(|v| smolscale::spawn(port_forwarder::port_forwarder(v.clone())))
            .collect();
        if !port_forwarders.is_empty() {
            smolscale::spawn(select_all(port_forwarders)).await;
        }

        Lazy::force(&stats::STATS_THREAD);

        // ready, set, go!
        Lazy::force(&vpn::VPN_SHUFFLE_TASK);
        socks5_fut.race(dns_fut).race(refresh_fut).await.unwrap();
        panic!("something died")
    })
});
