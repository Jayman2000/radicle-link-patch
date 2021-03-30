// Copyright © 2019-2020 The Radicle Foundation <hello@radicle.foundation>
//
// This file is part of radicle-link, distributed under the GPLv3 with Radicle
// Linking Exception. For full terms see the included LICENSE file.

use std::{
    fmt::Debug,
    future::Future,
    net::SocketAddr,
    ops::Deref,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use futures::{
    channel::mpsc,
    future::{BoxFuture, FutureExt as _, TryFutureExt as _},
    stream::StreamExt as _,
};
use governor::{Quota, RateLimiter};
use nonempty::NonEmpty;
use nonzero_ext::nonzero;
use rand_pcg::Pcg64Mcg;
use tracing::Instrument as _;

use super::{
    connection::{LocalAddr, LocalPeer},
    quic,
    upgrade,
    Network,
};
use crate::{
    git::{
        self,
        p2p::{
            server::GitServer,
            transport::{GitStream, GitStreamFactory},
        },
        replication,
        storage::{self, PoolError, PooledRef},
    },
    paths::Paths,
    signer::Signer,
    PeerId,
};

pub mod broadcast;
pub mod error;
pub mod event;
pub mod gossip;
pub mod interrogation;
pub mod membership;

mod info;
pub use info::{Capability, PartialPeerInfo, PeerAdvertisement, PeerInfo};

mod accept;
mod cache;
mod control;
mod io;
mod nonce;
mod tick;

mod tincans;
pub(super) use tincans::TinCans;
pub use tincans::{Interrogation, RecvError};

#[derive(Clone, Debug)]
pub struct Config {
    pub paths: Paths,
    pub listen_addr: SocketAddr,
    pub advertised_addrs: Option<NonEmpty<SocketAddr>>,
    pub membership: membership::Params,
    pub network: Network,
    pub replication: replication::Config,
    pub fetch: config::Fetch,
    // TODO: transport, ...
}

pub mod config {
    use std::time::Duration;

    #[derive(Clone, Copy, Debug)]
    pub struct Fetch {
        pub fetch_slot_wait_timeout: Duration,
    }

    impl Default for Fetch {
        fn default() -> Self {
            Self {
                fetch_slot_wait_timeout: Duration::from_secs(20),
            }
        }
    }
}

/// Binding of a peer to a network socket.
///
/// Created by [`crate::net::peer::Peer::bind`]. Call [`Bound::accept`] to start
/// accepting connections from peers.
pub struct Bound<S> {
    phone: TinCans,
    state: State<S>,
    incoming: quic::IncomingConnections<'static>,
    periodic: mpsc::Receiver<membership::Periodic<SocketAddr>>,
}

impl<S> Bound<S> {
    pub fn peer_id(&self) -> PeerId {
        self.state.local_id
    }

    pub fn listen_addrs(&self) -> std::io::Result<Vec<SocketAddr>> {
        self.state.endpoint.listen_addrs()
    }

    /// Start accepting connections from remote peers.
    ///
    /// Unbinds from the socket if the returned future is dropped.
    pub async fn accept<D>(self, disco: D) -> Result<!, quic::Error>
    where
        S: ProtocolStorage<SocketAddr, Update = gossip::Payload> + Clone + 'static,
        D: futures::Stream<Item = (PeerId, Vec<SocketAddr>)> + Send + 'static,
    {
        accept(self, disco).await
    }
}

impl<S> LocalPeer for Bound<S> {
    fn local_peer_id(&self) -> PeerId {
        self.peer_id()
    }
}

impl<S> LocalAddr for Bound<S> {
    type Addr = SocketAddr;

    fn listen_addrs(&self) -> std::io::Result<Vec<Self::Addr>> {
        self.state.endpoint.listen_addrs()
    }
}

pub async fn bind<Sign, Store>(
    phone: TinCans,
    config: Config,
    signer: Sign,
    storage: Store,
) -> Result<Bound<Store>, error::Bootstrap>
where
    Sign: Signer + Clone + Send + Sync + 'static,
    Store: ProtocolStorage<SocketAddr, Update = gossip::Payload> + Clone + 'static,
{
    let local_id = PeerId::from_signer(&signer);
    let git = GitServer::new(&config.paths);
    let quic::BoundEndpoint { endpoint, incoming } = quic::Endpoint::bind(
        signer,
        config.listen_addr,
        config.advertised_addrs,
        config.network,
    )
    .await?;
    let (membership, periodic) = membership::Hpv::<_, SocketAddr>::new(
        local_id,
        Pcg64Mcg::new(rand::random()),
        config.membership,
    );
    let storage = Storage::from(storage);
    let state = State {
        local_id,
        endpoint,
        git,
        membership,
        storage,
        phone: phone.clone(),
        config: StateConfig {
            replication: config.replication,
            fetch: config.fetch,
        },
        nonces: nonce::NonceBag::new(Duration::from_secs(300)), // TODO: config
        caches: cache::Caches::default(),
    };

    Ok(Bound {
        phone,
        state,
        incoming,
        periodic,
    })
}

pub fn accept<Store, Disco>(
    Bound {
        phone,
        state,
        incoming,
        periodic,
    }: Bound<Store>,
    disco: Disco,
) -> impl Future<Output = Result<!, quic::Error>>
where
    Store: ProtocolStorage<SocketAddr, Update = gossip::Payload> + Clone + 'static,
    Disco: futures::Stream<Item = (PeerId, Vec<SocketAddr>)> + Send + 'static,
{
    let _git_factory = Arc::new(Box::new(state.clone()) as Box<dyn GitStreamFactory>);
    git::p2p::transport::register()
        .register_stream_factory(state.local_id, Arc::downgrade(&_git_factory));

    let tasks = [
        tokio::spawn(accept::disco(state.clone(), disco)),
        tokio::spawn(accept::periodic(state.clone(), periodic)),
        tokio::spawn(accept::ground_control(
            state.clone(),
            async_stream::stream! {
                let mut r = phone.downstream.subscribe();
                loop { yield r.recv().await; }
            }
            .boxed(),
        )),
    ];
    let main = io::connections::incoming(state.clone(), incoming).boxed();

    Accept {
        _git_factory,
        endpoint: state.endpoint,
        tasks,
        main,
    }
}

struct Accept {
    _git_factory: Arc<Box<dyn GitStreamFactory>>,
    endpoint: quic::Endpoint,
    tasks: [tokio::task::JoinHandle<()>; 3],
    main: BoxFuture<'static, Result<!, quic::Error>>,
}

impl Future for Accept {
    type Output = Result<!, quic::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.main.poll_unpin(cx)
    }
}

impl Drop for Accept {
    fn drop(&mut self) {
        self.endpoint.shutdown();
        for task in &self.tasks {
            task.abort()
        }
    }
}

pub trait ProtocolStorage<A>: broadcast::LocalStorage<A> + storage::Pooled + Send + Sync {}
impl<A, T> ProtocolStorage<A> for T where
    T: broadcast::LocalStorage<A> + storage::Pooled + Send + Sync
{
}

type Limiter = governor::RateLimiter<
    governor::state::direct::NotKeyed,
    governor::state::InMemoryState,
    governor::clock::DefaultClock,
>;

#[derive(Clone)]
struct Storage<S> {
    inner: S,
    limiter: Arc<Limiter>,
}

impl<S> From<S> for Storage<S> {
    fn from(inner: S) -> Self {
        Self {
            inner,
            limiter: Arc::new(RateLimiter::direct(Quota::per_second(
                // TODO: make this an "advanced" config
                nonzero!(5u32),
            ))),
        }
    }
}

impl<S> Deref for Storage<S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[async_trait]
impl<A, S> broadcast::LocalStorage<A> for Storage<S>
where
    A: 'static,
    S: broadcast::LocalStorage<A>,
    S::Update: Send,
{
    type Update = S::Update;

    async fn put<P>(&self, provider: P, has: Self::Update) -> broadcast::PutResult<Self::Update>
    where
        P: Into<(PeerId, Vec<A>)> + Send,
    {
        self.inner.put(provider, has).await
    }

    async fn ask(&self, want: Self::Update) -> bool {
        self.inner.ask(want).await
    }
}

impl<S> broadcast::ErrorRateLimited for Storage<S> {
    fn is_error_rate_limit_breached(&self) -> bool {
        self.limiter.check().is_err()
    }
}

#[async_trait]
impl<S> storage::Pooled for Storage<S>
where
    S: storage::Pooled + Send + Sync,
{
    async fn get(&self) -> Result<PooledRef, PoolError> {
        self.inner.get().await
    }
}

#[derive(Clone, Copy)]
pub struct StateConfig {
    pub replication: replication::Config,
    pub fetch: config::Fetch,
}

#[derive(Clone)]
struct State<S> {
    local_id: PeerId,
    endpoint: quic::Endpoint,
    git: GitServer,
    membership: membership::Hpv<Pcg64Mcg, SocketAddr>,
    storage: Storage<S>,
    phone: TinCans,
    config: StateConfig,
    nonces: nonce::NonceBag,
    caches: cache::Caches,
}

#[async_trait]
impl<S> GitStreamFactory for State<S>
where
    S: ProtocolStorage<SocketAddr, Update = gossip::Payload> + Clone + 'static,
{
    async fn open_stream(
        &self,
        to: &PeerId,
        addr_hints: &[SocketAddr],
    ) -> Option<Box<dyn GitStream>> {
        let span = tracing::info_span!("open-git-stream", remote_id = %to);

        let may_conn = match self.endpoint.get_connection(*to) {
            Some(conn) => Some(conn),
            None => {
                let addr_hints = addr_hints.iter().copied().collect::<Vec<_>>();
                io::connect(&self.endpoint, *to, addr_hints)
                    .instrument(span.clone())
                    .await
                    .map(|(conn, ingress)| {
                        tokio::spawn(io::streams::incoming(self.clone(), ingress));
                        conn
                    })
            },
        };

        match may_conn {
            None => {
                span.in_scope(|| tracing::error!("unable to obtain connection"));
                None
            },

            Some(conn) => {
                let stream = conn
                    .open_bidi()
                    .inspect_err(|e| tracing::error!(err = ?e, "unable to open stream"))
                    .instrument(span.clone())
                    .await
                    .ok()?;
                let upgraded = upgrade::upgrade(stream, upgrade::Git)
                    .inspect_err(|e| tracing::error!(err = ?e, "unable to upgrade stream"))
                    .instrument(span)
                    .await
                    .ok()?;

                Some(Box::new(upgraded))
            },
        }
    }
}

impl<R, A> broadcast::Membership for membership::Hpv<R, A>
where
    R: rand::Rng + Clone,
    A: Clone + Debug + Ord,
{
    fn members(&self, exclude: Option<PeerId>) -> Vec<PeerId> {
        self.broadcast_recipients(exclude)
    }

    fn is_member(&self, peer: &PeerId) -> bool {
        self.is_known(peer)
    }
}
