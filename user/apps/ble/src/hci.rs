//! bt-hci's `Transport` over the kernel's `bt:hci` (doc/48 §3): one H4
//! packet per `read`/`write`.

use core::convert::Infallible;
use core::future::{Future, poll_fn};
use core::task::Poll;

use bt_hci::PacketKind;
use bt_hci::transport::{PacketToController, PacketToHost, Transport};
use bt_hci::ReadHciError;
use t5_rt::{Error, Handle, bt};

use crate::exec::want;

pub struct Hci {
    pub h: Handle,
}

#[derive(Debug, Clone, Copy)]
pub struct HciError;

impl core::fmt::Display for HciError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("bt:hci failed")
    }
}

impl core::error::Error for HciError {}

impl embedded_io::Error for HciError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

impl From<ReadHciError<Infallible>> for HciError {
    fn from(_: ReadHciError<Infallible>) -> Self {
        HciError
    }
}

impl embedded_io::ErrorType for Hci {
    type Error = HciError;
}

impl Transport for Hci {
    fn read<'a, P: PacketToHost<'a>>(&self, rx: &'a mut [u8]) -> impl Future<Output = Result<P, HciError>> {
        async move {
            let mut raw = [0u8; bt::PKT_MAX];
            let n = poll_fn(|_| match self.h.read(&mut raw) {
                Ok(n) => Poll::Ready(Ok(n)),
                Err(Error::Busy) => {
                    want(self.h.raw());
                    Poll::Pending
                }
                Err(_) => Poll::Ready(Err(HciError)),
            })
            .await?;
            // Events other than advertising reports (doc/48 §8: what the
            // controller says about connections).
            if n >= 4 && raw[0] == bt::EVT && !(raw[1] == 0x3E && matches!(raw[3], 0x02 | 0x0D)) && raw[1] != 0x0E && raw[1] != 0x13 {
                t5_rt::event!(Debug, "ble.hci_event", code = raw[1], head = &raw[..n.min(24)]);
            }
            let mut reader = &raw[..n];
            let kind = PacketKind::read(&mut reader)?;
            Ok(P::read_hci(kind, &mut reader, rx)?)
        }
    }

    fn write<P: PacketToController>(&self, tx: &P) -> impl Future<Output = Result<(), HciError>> {
        async move {
            let mut buf = [0u8; bt::PKT_MAX];
            let n = tx.size();
            if n + 1 > buf.len() {
                return Err(HciError);
            }
            buf[0] = P::KIND as u8;
            tx.write_hci(&mut buf[1..1 + n]).map_err(|_| HciError)?;
            loop {
                match self.h.write(&buf[..1 + n]) {
                    Ok(_) => return Ok(()),
                    // The chip has no room: try again shortly.
                    Err(Error::Busy) => embassy_time::Timer::after_millis(1).await,
                    Err(_) => return Err(HciError),
                }
            }
        }
    }
}
