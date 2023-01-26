use geph4_protocol::binder::protocol::{BridgeDescriptor, ExitDescriptor};

use itertools::Itertools;
use native_tls::{Protocol, TlsConnector};
use rand::Rng;
use regex::Regex;
use smol_timeout::TimeoutExt;
use sosistab2::{Multiplex, MuxPublic, MuxSecret, ObfsTlsPipe, ObfsUdpPipe, ObfsUdpPublic, Pipe};

use crate::connect::tunnel::TunnelStatus;

use super::{BinderTunnelParams, EndpointSource, TunnelCtx};
use anyhow::Context;
use std::{net::SocketAddr, sync::Weak};

use std::{convert::TryFrom, sync::Arc, time::Duration};

pub fn parse_independent_endpoint(endpoint: &str) -> anyhow::Result<(SocketAddr, [u8; 32])> {
    // parse endpoint addr
    let pk_and_url = endpoint.split('@').collect::<Vec<_>>();
    let server_pk = <[u8; 32]>::try_from(
        hex::decode(pk_and_url.first().context("URL not in form PK@host:port")?)
            .context("PK is not hex")?,
    )
    .ok()
    .context("cannot parse server pk")?;
    let server_addr: SocketAddr = pk_and_url
        .get(1)
        .context("URL not in form PK@host:port")?
        .parse()
        .context("cannot parse host:port")?;
    Ok((server_addr, server_pk))
}

pub(crate) async fn get_session(ctx: TunnelCtx) -> anyhow::Result<Arc<sosistab2::Multiplex>> {
    match &ctx.endpoint {
        EndpointSource::Independent { endpoint } => {
            let (addr, raw_key) = parse_independent_endpoint(endpoint)?;
            let obfs_pk = ObfsUdpPublic::from_bytes(raw_key);
            let sessid = rand::thread_rng().gen::<u128>().to_string();
            let mplex = Multiplex::new(MuxSecret::generate(), None);
            for _ in 0..4 {
                let pipe = ObfsUdpPipe::connect(addr, obfs_pk, &sessid).await?;
                mplex.add_pipe(pipe);
            }
            Ok(Arc::new(mplex))
        }
        EndpointSource::Binder(binder_tunnel_params) => {
            let selected_exit = binder_tunnel_params
                .ccache
                .get_closest_exit(&binder_tunnel_params.exit_server.clone().unwrap_or_default())
                .await
                .context("cannot get closest exit")?;
            log::info!("using exit {}", selected_exit.hostname);
            let bridges = binder_tunnel_params
                .ccache
                .get_bridges_v2(&selected_exit.hostname, false)
                .await
                .context("cannot get bridges")?;
            if bridges.is_empty() {
                anyhow::bail!("no sosistab2 routes to {}", selected_exit.hostname)
            }
            log::debug!("{} routes", bridges.len());
            // The bridge descriptor is laid out in a rather weird format: the "sosistab_key" field is a bincode-encode tuple of the first-level cookie, and the end-to-end MuxPublic key.
            // we assume we have at least one obfsudp key
            let e2e_key: MuxPublic = {
                let mut seen = None;
                for bridge in bridges.iter() {
                    if bridge.protocol == "sosistab2-obfsudp" {
                        if let Ok(val) =
                            bincode::deserialize::<(ObfsUdpPublic, MuxPublic)>(&bridge.sosistab_key)
                        {
                            seen = Some(val.1)
                        }
                    }
                }
                seen.context("cannot deduce the sosistab2 MuxPublic of this exit")?
            };
            let multiplex = Arc::new(sosistab2::Multiplex::new(
                MuxSecret::generate(),
                Some(e2e_key),
            ));
            // add *all* the bridges!
            let sess_id = format!("sess-{}", rand::thread_rng().gen::<u128>());
            for bridge in bridges.into_iter() {
                let sess_id = sess_id.clone();
                let multiplex = multiplex.clone();
                let ctx = ctx.clone();
                smolscale::spawn(async move {
                    match connect_once(ctx, bridge.clone(), &sess_id).await {
                        Ok(pipe) => {
                            log::debug!(
                                "add initial pipe {} / {}",
                                pipe.protocol(),
                                pipe.peer_addr()
                            );
                            multiplex.add_pipe(pipe);
                        }
                        Err(err) => {
                            log::warn!(
                                "pipe creation failed for {} ({}): {:?}",
                                bridge.endpoint,
                                bridge.protocol,
                                err
                            )
                        }
                    }
                })
                .detach();
            }

            // weak here to prevent a reference cycle!
            let weak_multiplex = Arc::downgrade(&multiplex);
            multiplex.add_drop_friend(smolscale::spawn(replace_dead(
                ctx.clone(),
                binder_tunnel_params.clone(),
                selected_exit,
                sess_id,
                weak_multiplex,
            )));

            Ok(multiplex)
        }
    }
}

async fn connect_once(
    ctx: TunnelCtx,
    desc: BridgeDescriptor,
    meta: &str,
) -> anyhow::Result<Box<dyn Pipe>> {
    if let EndpointSource::Binder(params) = &ctx.endpoint {
        if params.use_bridges && desc.is_direct {
            anyhow::bail!("skipping direct connection")
        }
        if let Some(regex) = &params.force_protocol {
            let compiled = Regex::new(regex).context("invalid protocol force")?;
            if !compiled.is_match(&desc.protocol) {
                anyhow::bail!("skipping non-matching protocol")
            }
        }
    }
    match desc.protocol.as_str() {
        "sosistab2-obfsudp" => {
            log::debug!("trying to connect to {}", desc.endpoint);
            (ctx.status_callback)(TunnelStatus::PreConnect {
                addr: desc.endpoint,
                protocol: "sosistab2-obfsudp".into(),
            });
            let keys: (ObfsUdpPublic, MuxPublic) =
                bincode::deserialize(&desc.sosistab_key).context("cannot decode keys")?;

            let connection = ObfsUdpPipe::connect(desc.endpoint, keys.0, meta)
                .timeout(Duration::from_secs(10))
                .await
                .context("pipe connection timeout")??;
            Ok(Box::new(connection))
        }
        "sosistab2-obfstls" => {
            let mut config = TlsConnector::builder();
            config
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .min_protocol_version(Some(Protocol::Tlsv12))
                .max_protocol_version(Some(Protocol::Tlsv12))
                .use_sni(true);
            let fake_domain = format!(
                "{}.{}{}.com",
                eff_wordlist::short::random_word(),
                eff_wordlist::large::random_word(),
                eff_wordlist::large::random_word()
            );
            let connection =
                ObfsTlsPipe::connect(desc.endpoint, &fake_domain, config, desc.sosistab_key, meta)
                    .timeout(Duration::from_secs(10))
                    .await
                    .context("pipe connection timeout")??;
            Ok(Box::new(connection))
        }
        other => {
            anyhow::bail!("unknown protocol {other}")
        }
    }
}

async fn replace_dead(
    ctx: TunnelCtx,
    binder_tunnel_params: BinderTunnelParams,
    selected_exit: ExitDescriptor,
    sess_id: String,
    weak_multiplex: Weak<Multiplex>,
) {
    let ccache = binder_tunnel_params.ccache.clone();
    loop {
        smol::Timer::after(Duration::from_secs(300)).await;
        loop {
            let fallible_part = async {
                let bridges = ccache.get_bridges_v2(&selected_exit.hostname, true).await?;
                let multiplex = weak_multiplex.upgrade().context("multiplex is dead")?;
                let current_pipes = multiplex.iter_pipes().collect_vec();
                let new_bridges = bridges
                    .into_iter()
                    .filter(|br| {
                        !current_pipes
                            .iter()
                            .any(|pipe| pipe.peer_addr() == br.endpoint.to_string())
                    })
                    .collect_vec();
                for bridge in new_bridges.into_iter() {
                    let sess_id = sess_id.clone();
                    let multiplex = multiplex.clone();
                    let ctx = ctx.clone();
                    smolscale::spawn(async move {
                        match connect_once(ctx, bridge.clone(), &sess_id).await {
                            Ok(pipe) => {
                                log::debug!(
                                    "add new pipe {} / {}",
                                    pipe.protocol(),
                                    pipe.peer_addr()
                                );
                                multiplex.add_pipe(pipe);
                            }
                            Err(err) => {
                                log::warn!(
                                    "pipe creation failed for {} ({}): {:?}",
                                    bridge.endpoint,
                                    bridge.protocol,
                                    err
                                )
                            }
                        }
                    })
                    .detach();
                }
                anyhow::Ok(())
            };
            if let Err(err) = fallible_part.await {
                log::warn!("error refreshing bridges: {:?}", err)
            }
        }
    }
}
