use std::{
    collections::HashMap, future::Future, marker::PhantomData, net::SocketAddr, pin::Pin, sync::Arc,
};

use ntp_proto::{
    NtpClock, NtpDuration, NtpInstant, NtpSource, NtpSourceActionIterator, NtpSourceUpdate,
    NtpTimestamp, ObservableSourceState, OneWaySourceUpdate, SourceController, SystemSourceUpdate,
};
#[cfg(target_os = "linux")]
use timestamped_socket::socket::open_interface_udp;
use timestamped_socket::{
    interface::InterfaceName,
    socket::{Connected, RecvResult, Socket, connect_address},
};
use tracing::{Instrument, Span, debug, error, instrument, warn};

use tokio::time::{Instant, Sleep};

use super::{config::TimestampMode, exitcode, spawn::SourceId, util::convert_net_timestamp};

/// Trait needed to allow injecting of futures other than `tokio::time::Sleep` for testing
pub trait Wait: Future<Output = ()> {
    fn reset(self: Pin<&mut Self>, deadline: Instant);
}

impl Wait for Sleep {
    fn reset(self: Pin<&mut Self>, deadline: Instant) {
        self.reset(deadline);
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum MsgForSystem<SourceMessage> {
    /// Received a Kiss-o'-Death and must demobilize
    MustDemobilize(SourceId),
    /// Experienced a network issue and must be restarted
    NetworkIssue(SourceId),
    /// Source is unreachable, and should be restarted with new resolved addr.
    Unreachable(SourceId),
    /// Update from source
    SourceUpdate(SourceId, NtpSourceUpdate<SourceMessage>),
    /// Update from sock source
    OneWaySourceUpdate(SourceId, OneWaySourceUpdate<SourceMessage>),
}

#[derive(Debug)]
pub struct SourceChannels<ControllerMessage, SourceMessage> {
    pub msg_for_system_sender: tokio::sync::mpsc::Sender<MsgForSystem<SourceMessage>>,
    pub system_update_receiver:
        tokio::sync::broadcast::Receiver<SystemSourceUpdate<ControllerMessage>>,
    pub source_snapshots:
        Arc<std::sync::RwLock<HashMap<SourceId, ObservableSourceState<SourceId>>>>,
}

pub(crate) struct SourceTask<
    C: 'static + NtpClock + Send,
    Controller: SourceController<MeasurementDelay = NtpDuration>,
    T: Wait,
> {
    _wait: PhantomData<T>,
    index: SourceId,
    clock: C,
    interface: Option<InterfaceName>,
    timestamp_mode: TimestampMode,
    name: String,
    source_addr: SocketAddr,
    socket: Option<Socket<SocketAddr, Connected>>,
    channels: SourceChannels<Controller::ControllerMessage, Controller::SourceMessage>,

    source: NtpSource<Controller>,

    // we don't store the real origin timestamp in the packet, because that would leak our
    // system time to the network (and could make attacks easier). So instead there is some
    // garbage data in the origin_timestamp field, and we need to track and pass along the
    // actual origin timestamp ourselves.
    /// Timestamp of the last packet that we sent
    last_send_timestamp: Option<NtpTimestamp>,
}

#[derive(Debug)]
enum SocketResult {
    Ok,
    Abort,
}

impl<C, Controller: SourceController<MeasurementDelay = NtpDuration>, T>
    SourceTask<C, Controller, T>
where
    C: 'static + NtpClock + Send + Sync,
    T: Wait,
{
    async fn setup_socket(&mut self) -> SocketResult {
        let socket_res = match self.interface {
            #[cfg(target_os = "linux")]
            Some(interface) => {
                open_interface_udp(
                    interface,
                    0, /*lets os choose*/
                    self.timestamp_mode.as_interface_mode(),
                    None,
                )
                .and_then(|socket| socket.connect(self.source_addr))
            }
            _ => connect_address(self.source_addr, self.timestamp_mode.as_general_mode()),
        };

        self.socket = match socket_res {
            Ok(socket) => Some(socket),
            Err(error) => {
                warn!(?error, "Could not open socket");
                return SocketResult::Abort;
            }
        };

        SocketResult::Ok
    }

    async fn run(&mut self, mut poll_wait: Pin<&mut T>) {
        loop {
            let mut buf = [0_u8; 1024];

            #[allow(clippy::large_enum_variant)]
            enum SelectResult<Controller: SourceController> {
                Timer,
                Recv(Result<RecvResult<SocketAddr>, std::io::Error>),
                SystemUpdate(
                    Result<
                        SystemSourceUpdate<Controller::ControllerMessage>,
                        tokio::sync::broadcast::error::RecvError,
                    >,
                ),
            }

            let selected: SelectResult<Controller> = tokio::select! {
                () = &mut poll_wait => {
                    SelectResult::Timer
                },
                result = self.channels.system_update_receiver.recv() => {
                    SelectResult::SystemUpdate(result)
                },
                result = async { if let Some(ref mut socket) = self.socket { socket.recv(&mut buf).await } else { std::future::pending().await }} => {
                    SelectResult::Recv(result)
                },
            };

            let actions = match selected {
                SelectResult::Recv(result) => {
                    tracing::debug!("accept packet");
                    match accept_packet(result, &buf, &self.clock) {
                        AcceptResult::Accept(packet, recv_timestamp) => {
                            let send_timestamp = match self.last_send_timestamp {
                                Some(ts) => ts,
                                None => {
                                    debug!(
                                        "we received a message without having sent one; discarding"
                                    );
                                    continue;
                                }
                            };
                            let actions = self.source.handle_incoming(
                                packet,
                                NtpInstant::now(),
                                send_timestamp,
                                recv_timestamp,
                            );
                            self.channels
                                .source_snapshots
                                .write()
                                .expect("Unexpected poisoned mutex")
                                .insert(
                                    self.index,
                                    self.source.observe(self.name.clone(), self.index),
                                );
                            actions
                        }
                        AcceptResult::NetworkGone => {
                            self.channels
                                .msg_for_system_sender
                                .send(MsgForSystem::NetworkIssue(self.index))
                                .await
                                .ok();
                            self.channels
                                .source_snapshots
                                .write()
                                .expect("Unexpected poisoned mutex")
                                .remove(&self.index);
                            return;
                        }
                        AcceptResult::Ignore => NtpSourceActionIterator::default(),
                    }
                }
                SelectResult::Timer => {
                    tracing::debug!("wait completed");
                    let actions = self.source.handle_timer();
                    self.channels
                        .source_snapshots
                        .write()
                        .expect("Unexpected poisoned mutex")
                        .insert(
                            self.index,
                            self.source.observe(self.name.clone(), self.index),
                        );
                    actions
                }
                SelectResult::SystemUpdate(result) => match result {
                    Ok(update) => {
                        let actions = self.source.handle_system_update(update);
                        self.channels
                            .source_snapshots
                            .write()
                            .expect("Unexpected poisoned mutex")
                            .insert(
                                self.index,
                                self.source.observe(self.name.clone(), self.index),
                            );
                        actions
                    }
                    Err(_) => NtpSourceActionIterator::default(),
                },
            };

            for action in actions {
                match action {
                    ntp_proto::NtpSourceAction::Send(packet) => {
                        if matches!(self.setup_socket().await, SocketResult::Abort) {
                            self.channels
                                .msg_for_system_sender
                                .send(MsgForSystem::NetworkIssue(self.index))
                                .await
                                .ok();
                            self.channels
                                .source_snapshots
                                .write()
                                .expect("Unexpected poisoned mutex")
                                .remove(&self.index);
                            return;
                        }

                        match self.clock.now() {
                            Err(e) => {
                                // we cannot determine the origin_timestamp
                                error!(error = ?e, "There was an error retrieving the current time");

                                // report as no permissions, since this seems the most likely
                                std::process::exit(exitcode::NOPERM);
                            }
                            Ok(ts) => {
                                self.last_send_timestamp = Some(ts);
                            }
                        }

                        match self.socket.as_mut().unwrap().send(&packet).await {
                            Err(error) => {
                                warn!(?error, "poll message could not be sent");

                                match error.raw_os_error() {
                                    Some(libc::EHOSTDOWN)
                                    | Some(libc::EHOSTUNREACH)
                                    | Some(libc::ENETDOWN)
                                    | Some(libc::ENETUNREACH) => {
                                        self.channels
                                            .msg_for_system_sender
                                            .send(MsgForSystem::NetworkIssue(self.index))
                                            .await
                                            .ok();
                                        self.channels
                                            .source_snapshots
                                            .write()
                                            .expect("Unexpected poisoned mutex")
                                            .remove(&self.index);
                                        return;
                                    }
                                    _ => {}
                                }
                            }
                            Ok(opt_send_timestamp) => {
                                // update the last_send_timestamp with the one given by the kernel, if available
                                self.last_send_timestamp = opt_send_timestamp
                                    .map(convert_net_timestamp)
                                    .or(self.last_send_timestamp);
                            }
                        }
                    }
                    ntp_proto::NtpSourceAction::UpdateSystem(update) => {
                        self.channels
                            .msg_for_system_sender
                            .send(MsgForSystem::SourceUpdate(self.index, update))
                            .await
                            .ok();
                    }
                    ntp_proto::NtpSourceAction::SetTimer(timeout) => {
                        if let Some(deadline) = Instant::now().checked_add(timeout) {
                            // If it overflows, it is so far in the future we may as well not set the timer.
                            poll_wait.as_mut().reset(deadline);
                        }
                    }
                    ntp_proto::NtpSourceAction::Reset => {
                        self.channels
                            .msg_for_system_sender
                            .send(MsgForSystem::Unreachable(self.index))
                            .await
                            .ok();
                        self.channels
                            .source_snapshots
                            .write()
                            .expect("Unexpected poisoned mutex")
                            .remove(&self.index);
                        return;
                    }
                    ntp_proto::NtpSourceAction::Demobilize => {
                        self.channels
                            .msg_for_system_sender
                            .send(MsgForSystem::MustDemobilize(self.index))
                            .await
                            .ok();
                        self.channels
                            .source_snapshots
                            .write()
                            .expect("Unexpected poisoned mutex")
                            .remove(&self.index);
                        return;
                    }
                }
            }
        }
    }
}

impl<C, Controller: SourceController<MeasurementDelay = NtpDuration>>
    SourceTask<C, Controller, Sleep>
where
    C: 'static + NtpClock + Send + Sync,
{
    #[allow(clippy::too_many_arguments)]
    #[instrument(level = tracing::Level::ERROR, name = "Ntp Source", skip(timestamp_mode, clock, channels, source, initial_actions))]
    pub fn spawn(
        index: SourceId,
        name: String,
        source_addr: SocketAddr,
        interface: Option<InterfaceName>,
        clock: C,
        timestamp_mode: TimestampMode,
        channels: SourceChannels<Controller::ControllerMessage, Controller::SourceMessage>,
        source: NtpSource<Controller>,
        initial_actions: NtpSourceActionIterator<Controller::SourceMessage>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(
            (async move {
                let poll_wait = tokio::time::sleep(std::time::Duration::default());
                tokio::pin!(poll_wait);

                for action in initial_actions {
                    match action {
                        ntp_proto::NtpSourceAction::Send(_) => {
                            unreachable!("Should not be sending messages from startup")
                        }
                        ntp_proto::NtpSourceAction::UpdateSystem(_) => {
                            unreachable!("Should not be updating system from startup")
                        }
                        ntp_proto::NtpSourceAction::SetTimer(timeout) => {
                            poll_wait.as_mut().reset(Instant::now() + timeout)
                        }
                        ntp_proto::NtpSourceAction::Reset => {
                            unreachable!("Should not be resetting from startup")
                        }
                        ntp_proto::NtpSourceAction::Demobilize => {
                            todo!("Should not be demobilizing from startup")
                        }
                    }
                }

                let mut process = SourceTask {
                    _wait: PhantomData,
                    index,
                    name,
                    clock,
                    channels,
                    interface,
                    timestamp_mode,
                    source_addr,
                    socket: None,
                    source,
                    last_send_timestamp: None,
                };

                process.run(poll_wait).await;
            })
            .instrument(Span::current()),
        )
    }
}

#[derive(Debug)]
enum AcceptResult<'a> {
    Accept(&'a [u8], NtpTimestamp),
    Ignore,
    NetworkGone,
}

fn accept_packet<'a, C: NtpClock>(
    result: Result<RecvResult<SocketAddr>, std::io::Error>,
    buf: &'a [u8],
    clock: &C,
) -> AcceptResult<'a> {
    match result {
        Ok(RecvResult {
            bytes_read: size,
            timestamp,
            ..
        }) => {
            let recv_timestamp =
                timestamp
                    .map(convert_net_timestamp)
                    .unwrap_or_else(|| match clock.now() {
                        Ok(now) => {
                            debug!(?size, "received a packet without a timestamp, substituting");
                            now
                        }
                        _ => {
                            panic!("Received packet without timestamp and couldn't substitute");
                        }
                    });

            // Note: packets are allowed to be bigger when including extensions.
            // we don't expect them, but the server may still send them. The
            // extra bytes are guaranteed safe to ignore. `recv` truncates the messages.
            // Messages of fewer than 48 bytes are skipped entirely
            if size < 48 {
                debug!(expected = 48, actual = size, "received packet is too small");

                AcceptResult::Ignore
            } else {
                AcceptResult::Accept(&buf[0..size], recv_timestamp)
            }
        }
        Err(receive_error) => {
            warn!(?receive_error, "could not receive packet");

            match receive_error.raw_os_error() {
                Some(libc::EHOSTDOWN)
                | Some(libc::EHOSTUNREACH)
                | Some(libc::ENETDOWN)
                | Some(libc::ENETUNREACH) => AcceptResult::NetworkGone,
                _ => AcceptResult::Ignore,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        net::Ipv4Addr,
        sync::{Arc, RwLock},
        time::Duration,
    };

    use ntp_proto::{
        AlgorithmConfig, KalmanClockController, KalmanControllerMessage, KalmanSourceMessage,
        NoCipher, NtpDuration, NtpLeapIndicator, NtpPacket, ProtocolVersion, SourceConfig,
        SynchronizationConfig, SystemSnapshot, TimeSnapshot, TwoWayKalmanSourceController,
    };
    use timestamped_socket::socket::{GeneralTimestampMode, Open, open_ip};
    use tokio::sync::{broadcast, mpsc};

    use crate::{daemon::util::EPOCH_OFFSET, test::alloc_port};

    use super::*;

    struct TestWaitSender {
        state: Arc<std::sync::Mutex<TestWaitState>>,
    }

    impl TestWaitSender {
        fn notify(&self) {
            let mut state = self.state.lock().unwrap();
            state.pending = true;
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }
    }

    struct TestWait {
        state: Arc<std::sync::Mutex<TestWaitState>>,
    }

    struct TestWaitState {
        waker: Option<std::task::Waker>,
        pending: bool,
    }

    impl Future for TestWait {
        type Output = ();

        fn poll(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            let mut state = self.state.lock().unwrap();

            if state.pending {
                state.pending = false;
                state.waker = None;
                std::task::Poll::Ready(())
            } else {
                state.waker = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        }
    }

    impl Wait for TestWait {
        fn reset(self: Pin<&mut Self>, _deadline: Instant) {}
    }

    impl Drop for TestWait {
        fn drop(&mut self) {
            self.state.lock().unwrap().waker = None;
        }
    }

    impl TestWait {
        fn new() -> (TestWait, TestWaitSender) {
            let state = Arc::new(std::sync::Mutex::new(TestWaitState {
                waker: None,
                pending: false,
            }));

            (
                TestWait {
                    state: state.clone(),
                },
                TestWaitSender { state },
            )
        }
    }

    #[derive(Debug, Clone, Default)]
    struct TestClock {}

    impl NtpClock for TestClock {
        type Error = std::time::SystemTimeError;

        fn now(&self) -> std::result::Result<NtpTimestamp, Self::Error> {
            let cur =
                std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH)?;

            Ok(NtpTimestamp::from_seconds_nanos_since_ntp_era(
                EPOCH_OFFSET.wrapping_add(cur.as_secs() as u32),
                cur.subsec_nanos(),
            ))
        }

        fn set_frequency(&self, _freq: f64) -> Result<NtpTimestamp, Self::Error> {
            self.now()
            //ignore
        }

        fn get_frequency(&self) -> Result<f64, Self::Error> {
            Ok(0.0)
        }

        fn step_clock(&self, _offset: NtpDuration) -> Result<NtpTimestamp, Self::Error> {
            panic!("Shouldn't be called by source");
        }

        fn disable_ntp_algorithm(&self) -> Result<(), Self::Error> {
            Ok(())
            //ignore
        }

        fn error_estimate_update(
            &self,
            _est_error: NtpDuration,
            _max_error: NtpDuration,
        ) -> Result<(), Self::Error> {
            panic!("Shouldn't be called by source");
        }

        fn status_update(&self, _leap_status: NtpLeapIndicator) -> Result<(), Self::Error> {
            Ok(())
            //ignore
        }
    }

    async fn test_startup<T: Wait>() -> (
        SourceTask<TestClock, TwoWayKalmanSourceController<SourceId>, T>,
        Socket<SocketAddr, Open>,
        mpsc::Receiver<MsgForSystem<KalmanSourceMessage<SourceId>>>,
        broadcast::Sender<SystemSourceUpdate<KalmanControllerMessage>>,
    ) {
        let port_base = alloc_port();
        let test_socket = open_ip(
            SocketAddr::from((Ipv4Addr::LOCALHOST, port_base)),
            GeneralTimestampMode::SoftwareRecv,
        )
        .unwrap();

        let (system_update_sender, system_update_receiver) = tokio::sync::broadcast::channel(1);
        let (msg_for_system_sender, msg_for_system_receiver) = mpsc::channel(1);

        let index = SourceId::new();
        let mut system: ntp_proto::System<_, KalmanClockController<_, _>> = ntp_proto::System::new(
            TestClock {},
            SynchronizationConfig::default(),
            AlgorithmConfig::default(),
            Arc::new([]),
        )
        .unwrap();

        let Ok((source, _)) = system.create_ntp_source(
            index,
            SourceConfig::default(),
            SocketAddr::from((Ipv4Addr::LOCALHOST, port_base)),
            ProtocolVersion::V4,
            None,
        ) else {
            panic!("Could not create test source");
        };

        let process = SourceTask {
            _wait: PhantomData,
            index,
            name: "test".into(),
            clock: TestClock {},
            channels: SourceChannels {
                msg_for_system_sender,
                system_update_receiver,
                source_snapshots: Arc::new(RwLock::new(HashMap::new())),
            },
            source_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port_base)),
            interface: None,
            timestamp_mode: TimestampMode::KernelRecv,
            socket: None,
            source,
            last_send_timestamp: None,
        };

        (
            process,
            test_socket,
            msg_for_system_receiver,
            system_update_sender,
        )
    }

    #[tokio::test]
    async fn test_poll_sends_state_update_and_packet() {
        // Note: Ports must be unique among tests to deal with parallelism
        let (mut process, socket, _, _system_update_sender) = test_startup().await;

        let (poll_wait, poll_send) = TestWait::new();

        let handle = tokio::spawn(async move {
            tokio::pin!(poll_wait);
            process.run(poll_wait).await;
        });

        poll_send.notify();

        let mut buf = [0; 48];
        let network = socket.recv(&mut buf).await.unwrap();
        assert_eq!(network.bytes_read, 48);

        handle.abort();
    }

    fn serialize_packet_unencrypted(send_packet: &NtpPacket) -> [u8; 48] {
        let mut buf = [0; 48];
        let mut cursor = Cursor::new(buf.as_mut_slice());
        send_packet.serialize(&mut cursor, &NoCipher, None).unwrap();

        assert_eq!(cursor.position(), 48);

        buf
    }

    #[tokio::test]
    async fn test_timeroundtrip() {
        // Note: Ports must be unique among tests to deal with parallelism
        let (mut process, mut socket, mut msg_recv, _system_update_sender) = test_startup().await;

        let system = SystemSnapshot {
            time_snapshot: TimeSnapshot {
                leap_indicator: NtpLeapIndicator::NoWarning,
                ..Default::default()
            },
            ..Default::default()
        };

        let (poll_wait, poll_send) = TestWait::new();
        let clock = TestClock {};

        let handle = tokio::spawn(async move {
            tokio::pin!(poll_wait);
            process.run(poll_wait).await;
        });

        poll_send.notify();

        let mut buf = [0; 48];
        let RecvResult {
            bytes_read: size,
            timestamp,
            remote_addr,
        } = socket.recv(&mut buf).await.unwrap();
        assert_eq!(size, 48);
        let timestamp = timestamp.unwrap();

        let rec_packet = NtpPacket::deserialize(&buf, &NoCipher).unwrap().0;
        let send_packet = NtpPacket::timestamp_response(
            &system,
            rec_packet,
            convert_net_timestamp(timestamp),
            &clock,
        );

        let serialized = serialize_packet_unencrypted(&send_packet);
        socket.send_to(&serialized, remote_addr).await.unwrap();

        let msg = msg_recv.recv().await.unwrap();
        assert!(matches!(msg, MsgForSystem::SourceUpdate(_, _)));

        handle.abort();
    }

    #[tokio::test]
    async fn test_deny_stops_poll() {
        // Note: Ports must be unique among tests to deal with parallelism
        let (mut process, mut socket, mut msg_recv, _system_update_sender) = test_startup().await;

        let (poll_wait, poll_send) = TestWait::new();

        let handle = tokio::spawn(async move {
            tokio::pin!(poll_wait);
            process.run(poll_wait).await;
        });

        for _ in 0..3 {
            poll_send.notify();

            let mut buf = [0; 48];
            let RecvResult {
                bytes_read: size,
                timestamp,
                remote_addr,
            } = socket.recv(&mut buf).await.unwrap();
            assert_eq!(size, 48);
            assert!(timestamp.is_some());

            let rec_packet = NtpPacket::deserialize(&buf, &NoCipher).unwrap().0;
            let send_packet = NtpPacket::deny_response(rec_packet);
            let serialized = serialize_packet_unencrypted(&send_packet);

            // Flush earlier messages
            while msg_recv.try_recv().is_ok() {}

            socket
                .send_to(&serialized, std::dbg!(remote_addr))
                .await
                .unwrap();

            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        poll_send.notify();

        let msg = dbg!(msg_recv.recv().await.unwrap());
        assert!(matches!(msg, MsgForSystem::MustDemobilize(_)));

        let mut buf = [0; 48];
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(10)) => {/*expected */},
            _ = socket.recv(&mut buf) => { unreachable!("should not receive anything") }
        }

        handle.abort();
    }
}
