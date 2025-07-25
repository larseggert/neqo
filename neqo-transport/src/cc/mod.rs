// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Congestion control

use std::{
    fmt::{Debug, Display},
    str::FromStr,
    time::{Duration, Instant},
};

use neqo_common::qlog::Qlog;

use crate::{recovery::sent, rtt::RttEstimate, Error, Pmtud};

mod classic_cc;
mod cubic;
mod new_reno;

pub use classic_cc::ClassicCongestionControl;
#[cfg(test)]
pub use classic_cc::CWND_INITIAL_PKTS;
pub use cubic::Cubic;
pub use new_reno::NewReno;

pub trait CongestionControl: Display + Debug {
    fn set_qlog(&mut self, qlog: Qlog);

    #[must_use]
    fn cwnd(&self) -> usize;

    #[must_use]
    fn bytes_in_flight(&self) -> usize;

    #[must_use]
    fn cwnd_avail(&self) -> usize;

    #[must_use]
    fn cwnd_min(&self) -> usize;

    #[cfg(test)]
    #[must_use]
    fn cwnd_initial(&self) -> usize;

    #[must_use]
    fn pmtud(&self) -> &Pmtud;

    #[must_use]
    fn pmtud_mut(&mut self) -> &mut Pmtud;

    fn on_packets_acked(
        &mut self,
        acked_pkts: &[sent::Packet],
        rtt_est: &RttEstimate,
        now: Instant,
    );

    /// Returns true if the congestion window was reduced.
    fn on_packets_lost(
        &mut self,
        first_rtt_sample_time: Option<Instant>,
        prev_largest_acked_sent: Option<Instant>,
        pto: Duration,
        lost_packets: &[sent::Packet],
        now: Instant,
    ) -> bool;

    /// Returns true if the congestion window was reduced.
    fn on_ecn_ce_received(&mut self, largest_acked_pkt: &sent::Packet, now: Instant) -> bool;

    #[must_use]
    fn recovery_packet(&self) -> bool;

    fn discard(&mut self, pkt: &sent::Packet, now: Instant);

    fn on_packet_sent(&mut self, pkt: &sent::Packet, now: Instant);

    fn discard_in_flight(&mut self, now: Instant);
}

#[derive(Debug, Copy, Clone, Default)]
pub enum CongestionControlAlgorithm {
    NewReno,
    #[default]
    Cubic,
}

// A `FromStr` implementation so that this can be used in command-line interfaces.
impl FromStr for CongestionControlAlgorithm {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "newreno" | "reno" => Ok(Self::NewReno),
            "cubic" => Ok(Self::Cubic),
            _ => Err(Error::InvalidInput),
        }
    }
}

#[cfg(test)]
mod tests;
