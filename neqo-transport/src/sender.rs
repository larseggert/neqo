// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Congestion control

use std::{
    fmt::{self, Display},
    time::{Duration, Instant},
};

use neqo_common::{qdebug, qlog::Qlog};

use crate::{
    cc::{ClassicCongestionControl, CongestionControl, CongestionControlAlgorithm, Cubic, NewReno},
    pace::Pacer,
    pmtud::Pmtud,
    recovery::sent,
    rtt::RttEstimate,
    ConnectionParameters, Stats,
};

/// The number of packets we allow to burst from the pacer.
pub const PACING_BURST_SIZE: usize = 2;

#[derive(Debug)]
pub struct PacketSender {
    cc: Box<dyn CongestionControl>,
    pacer: Pacer,
}

impl Display for PacketSender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} {}", self.cc, self.pacer)
    }
}

impl PacketSender {
    #[must_use]
    pub fn new(conn_params: &ConnectionParameters, pmtud: Pmtud, now: Instant) -> Self {
        let mtu = pmtud.plpmtu();
        Self {
            cc: match conn_params.get_cc_algorithm() {
                CongestionControlAlgorithm::NewReno => {
                    Box::new(ClassicCongestionControl::new(NewReno::default(), pmtud))
                }
                CongestionControlAlgorithm::Cubic => {
                    Box::new(ClassicCongestionControl::new(Cubic::default(), pmtud))
                }
            },
            pacer: Pacer::new(
                conn_params.pacing_enabled(),
                now,
                mtu * PACING_BURST_SIZE,
                mtu,
            ),
        }
    }

    pub fn set_qlog(&mut self, qlog: Qlog) {
        self.cc.set_qlog(qlog);
    }

    pub fn pmtud(&self) -> &Pmtud {
        self.cc.pmtud()
    }

    pub fn pmtud_mut(&mut self) -> &mut Pmtud {
        self.cc.pmtud_mut()
    }

    #[must_use]
    pub fn cwnd(&self) -> usize {
        self.cc.cwnd()
    }

    #[must_use]
    pub fn cwnd_avail(&self) -> usize {
        self.cc.cwnd_avail()
    }

    #[cfg(test)]
    #[must_use]
    pub fn cwnd_min(&self) -> usize {
        self.cc.cwnd_min()
    }

    fn maybe_update_pacer_mtu(&mut self) {
        let current_mtu = self.pmtud().plpmtu();
        if current_mtu != self.pacer.mtu() {
            qdebug!(
                "PLPMTU changed from {} to {current_mtu}, updating pacer",
                self.pacer.mtu()
            );
            self.pacer.set_mtu(current_mtu);
        }
    }

    pub fn on_packets_acked(
        &mut self,
        acked_pkts: &[sent::Packet],
        rtt_est: &RttEstimate,
        now: Instant,
        stats: &mut Stats,
    ) {
        self.cc.on_packets_acked(acked_pkts, rtt_est, now);
        self.pmtud_mut().on_packets_acked(acked_pkts, now, stats);
        self.maybe_update_pacer_mtu();
    }

    /// Called when packets are lost.  Returns true if the congestion window was reduced.
    pub fn on_packets_lost(
        &mut self,
        first_rtt_sample_time: Option<Instant>,
        prev_largest_acked_sent: Option<Instant>,
        pto: Duration,
        lost_packets: &[sent::Packet],
        stats: &mut Stats,
        now: Instant,
    ) -> bool {
        let ret = self.cc.on_packets_lost(
            first_rtt_sample_time,
            prev_largest_acked_sent,
            pto,
            lost_packets,
            now,
        );
        // Call below may change the size of MTU probes, so it needs to happen after the CC
        // reaction above, which needs to ignore probes based on their size.
        self.pmtud_mut().on_packets_lost(lost_packets, stats, now);
        self.maybe_update_pacer_mtu();
        ret
    }

    /// Called when ECN CE mark received.  Returns true if the congestion window was reduced.
    pub fn on_ecn_ce_received(&mut self, largest_acked_pkt: &sent::Packet, now: Instant) -> bool {
        self.cc.on_ecn_ce_received(largest_acked_pkt, now)
    }

    pub fn discard(&mut self, pkt: &sent::Packet, now: Instant) {
        self.cc.discard(pkt, now);
    }

    /// When we migrate, the congestion controller for the previously active path drops
    /// all bytes in flight.
    pub fn discard_in_flight(&mut self, now: Instant) {
        self.cc.discard_in_flight(now);
    }

    pub fn on_packet_sent(&mut self, pkt: &sent::Packet, rtt: Duration, now: Instant) {
        self.pacer
            .spend(pkt.time_sent(), rtt, self.cc.cwnd(), pkt.len());
        self.cc.on_packet_sent(pkt, now);
    }

    #[must_use]
    pub fn next_paced(&self, rtt: Duration) -> Option<Instant> {
        // Only pace if there are bytes in flight.
        (self.cc.bytes_in_flight() > 0).then(|| self.pacer.next(rtt, self.cc.cwnd()))
    }

    #[must_use]
    pub fn recovery_packet(&self) -> bool {
        self.cc.recovery_packet()
    }
}
