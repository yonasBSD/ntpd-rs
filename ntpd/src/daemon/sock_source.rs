use std::{fmt::Display, path::Path};

use ntp_proto::{
    Measurement, NtpClock, NtpDuration, NtpInstant, NtpLeapIndicator, ReferenceId,
    SockSourceSnapshot, SockSourceUpdate, SourceController, SystemSourceUpdate,
};
use tracing::debug;
#[cfg(target_os = "linux")]
use tracing::{error, instrument, Instrument, Span};

use tokio::net::UnixDatagram;

use crate::daemon::{exitcode, ntp_source::MsgForSystem};

use super::{ntp_source::SourceChannels, spawn::SourceId};

// Based on https://gitlab.com/gpsd/gpsd/-/blob/master/gpsd/timehint.c#L268
#[derive(Debug)]
struct SockSample {
    // tv_sec: i64,
    // tv_usec: i64,
    offset: f64,
    pulse: i32,
    leap: i32,
    magic: i32,
}

const SOCK_MAGIC: i32 = 0x534f434b;
const SOCK_SAMPLE_SIZE: usize = 40;

#[derive(Debug)]
enum SampleError {
    IOError(std::io::Error),
    SliceError(std::array::TryFromSliceError),
    WrongSize(usize),
    WrongMagic(i32),
    WrongPulse(i32),
}

impl Display for SampleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SampleError::IOError(e) => f.write_str(&e.to_string()),
            SampleError::SliceError(e) => f.write_str(&e.to_string()),
            SampleError::WrongSize(s) => f.write_fmt(format_args!("Invalid size {s}")),
            SampleError::WrongMagic(m) => f.write_fmt(format_args!("Invalid magic {m}")),
            SampleError::WrongPulse(p) => f.write_fmt(format_args!("Invalid pulse {p}")),
        }
    }
}

fn deserialize_sample(
    result: Result<usize, std::io::Error>,
    buf: [u8; SOCK_SAMPLE_SIZE],
) -> Result<SockSample, SampleError> {
    let size = result.map_err(SampleError::IOError)?;
    if size != SOCK_SAMPLE_SIZE {
        return Err(SampleError::WrongSize(size));
    }

    let sample = SockSample {
        // tv_sec: i64::from_le_bytes(buf[0..8].try_into()?),
        // tv_usec: i64::from_le_bytes(buf[8..16].try_into()?),
        offset: f64::from_le_bytes(buf[16..24].try_into().map_err(SampleError::SliceError)?),
        pulse: i32::from_le_bytes(buf[24..28].try_into().map_err(SampleError::SliceError)?),
        leap: i32::from_le_bytes(buf[28..32].try_into().map_err(SampleError::SliceError)?),
        // skip padding (4 bytes)
        magic: i32::from_le_bytes(buf[36..40].try_into().map_err(SampleError::SliceError)?),
    };

    if sample.magic != SOCK_MAGIC {
        return Err(SampleError::WrongMagic(sample.magic));
    }

    if sample.pulse != 0 {
        return Err(SampleError::WrongPulse(sample.pulse));
    }

    Ok(sample)
}

pub(crate) struct SockSourceTask<C: 'static + NtpClock + Send, Controller: SourceController> {
    index: SourceId,
    socket: UnixDatagram,
    clock: C,
    channels: SourceChannels<Controller::ControllerMessage, Controller::SourceMessage>,
    controller: Controller,
}

async fn create_socket(socket_path: String) -> std::io::Result<UnixDatagram> {
    let path = Path::new(&socket_path).to_path_buf();
    if path.exists() {
        debug!("Removing previous socket file");
        std::fs::remove_file(&path)?;
    }
    debug!("Creating socket at {:?}", path);
    let socket = UnixDatagram::bind(path)?;
    Ok(socket)
}

impl<C, Controller: SourceController<MeasurementDelay = ()>> SockSourceTask<C, Controller>
where
    C: 'static + NtpClock + Send + Sync,
{
    async fn run(&mut self) {
        loop {
            let mut buf = [0; SOCK_SAMPLE_SIZE];

            enum SelectResult<Controller: SourceController> {
                SockRecv(Result<usize, std::io::Error>),
                SystemUpdate(
                    Result<
                        SystemSourceUpdate<Controller::ControllerMessage>,
                        tokio::sync::broadcast::error::RecvError,
                    >,
                ),
            }

            let selected: SelectResult<Controller> = tokio::select! {
                result = self.socket.recv(&mut buf) => {
                    SelectResult::SockRecv(result)
                },
                result = self.channels.system_update_receiver.recv() => {
                    SelectResult::SystemUpdate(result)
                }
            };

            match selected {
                SelectResult::SockRecv(result) => match deserialize_sample(result, buf) {
                    Ok(sample) => {
                        debug!("received {:?}", sample);
                        let leap = match sample.leap {
                            0 => NtpLeapIndicator::NoWarning,
                            1 => NtpLeapIndicator::Leap61,
                            2 => NtpLeapIndicator::Leap59,
                            _ => NtpLeapIndicator::Unknown,
                        };

                        let time = match self.clock.now() {
                            Ok(time) => time,
                            Err(e) => {
                                error!(error = ?e, "There was an error retrieving the current time");
                                std::process::exit(exitcode::NOPERM);
                            }
                        };

                        let measurement = Measurement {
                            delay: (),
                            offset: NtpDuration::from_seconds(sample.offset),
                            localtime: time,
                            monotime: NtpInstant::now(),

                            stratum: 0,
                            root_delay: NtpDuration::ZERO,
                            root_dispersion: NtpDuration::ZERO,
                            leap,
                            precision: 0, // TODO: compute on startup?
                        };

                        let controller_message = self.controller.handle_measurement(measurement);

                        let update = SockSourceUpdate {
                            snapshot: SockSourceSnapshot {
                                source_id: ReferenceId::SOCK,
                                stratum: 0,
                            },
                            message: controller_message,
                        };
                        self.channels
                            .msg_for_system_sender
                            .send(MsgForSystem::SockSourceUpdate(self.index, update))
                            .await
                            .ok();
                    }
                    Err(e) => {
                        error!("Error deserializing sample: {}", e);
                        continue;
                    }
                },
                SelectResult::SystemUpdate(result) => match result {
                    Ok(update) => {
                        self.controller.handle_message(update.message);
                    }
                    Err(e) => {
                        error!("Error receiving system update: {:?}", e)
                    }
                },
            };
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(level = tracing::Level::ERROR, name = "Sock Source", skip(clock, channels, controller))]
    pub fn spawn(
        index: SourceId,
        socket_path: String,
        clock: C,
        channels: SourceChannels<Controller::ControllerMessage, Controller::SourceMessage>,
        controller: Controller,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(
            (async move {
                let socket = create_socket(socket_path)
                    .await
                    .expect("Could not create socket");

                let mut process = SockSourceTask {
                    index,
                    socket,
                    clock,
                    channels,
                    controller,
                };

                process.run().await;
            })
            .instrument(Span::current()),
        )
    }
}
