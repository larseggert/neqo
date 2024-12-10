// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Tracking of received packets and generating ACKs thereof.

use std::{
    cmp::min,
    collections::VecDeque,
    ops::{Index, IndexMut},
    time::{Duration, Instant},
};

use enum_map::{enum_map, Enum, EnumMap};
use neqo_common::{qdebug, qinfo, qtrace, qwarn, IpTosEcn};
use neqo_crypto::{Epoch, TLS_EPOCH_HANDSHAKE, TLS_EPOCH_INITIAL};

use crate::{
    ecn::EcnCount,
    frame::{FRAME_TYPE_ACK, FRAME_TYPE_ACK_ECN},
    packet::{PacketBuilder, PacketNumber, PacketType},
    recovery::RecoveryToken,
    stats::FrameStats,
};

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Ord, Eq, Enum)]
pub enum PacketNumberSpace {
    Initial,
    Handshake,
    ApplicationData,
}

#[allow(clippy::use_self)] // https://github.com/rust-lang/rust-clippy/issues/3410
impl PacketNumberSpace {
    pub fn iter() -> impl Iterator<Item = &'static PacketNumberSpace> {
        const SPACES: &[PacketNumberSpace] = &[
            PacketNumberSpace::Initial,
            PacketNumberSpace::Handshake,
            PacketNumberSpace::ApplicationData,
        ];
        SPACES.iter()
    }
}

impl From<Epoch> for PacketNumberSpace {
    fn from(epoch: Epoch) -> Self {
        match epoch {
            TLS_EPOCH_INITIAL => Self::Initial,
            TLS_EPOCH_HANDSHAKE => Self::Handshake,
            _ => Self::ApplicationData,
        }
    }
}

#[allow(clippy::fallible_impl_from)]
impl From<PacketType> for PacketNumberSpace {
    fn from(pt: PacketType) -> Self {
        match pt {
            PacketType::Initial => Self::Initial,
            PacketType::Handshake => Self::Handshake,
            PacketType::ZeroRtt | PacketType::Short => Self::ApplicationData,
            _ => panic!("Attempted to get space from wrong packet type"),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct PacketNumberSpaceSet {
    spaces: EnumMap<PacketNumberSpace, bool>,
}

impl PacketNumberSpaceSet {
    pub fn all() -> Self {
        Self {
            spaces: enum_map! {
                PacketNumberSpace::Initial => true,
                PacketNumberSpace::Handshake => true,
                PacketNumberSpace::ApplicationData => true,
            },
        }
    }
}

impl Index<PacketNumberSpace> for PacketNumberSpaceSet {
    type Output = bool;

    fn index(&self, space: PacketNumberSpace) -> &Self::Output {
        &self.spaces[space]
    }
}

impl IndexMut<PacketNumberSpace> for PacketNumberSpaceSet {
    fn index_mut(&mut self, space: PacketNumberSpace) -> &mut Self::Output {
        &mut self.spaces[space]
    }
}

impl<T: AsRef<[PacketNumberSpace]>> From<T> for PacketNumberSpaceSet {
    fn from(spaces: T) -> Self {
        let mut v = Self::default();
        for sp in spaces.as_ref() {
            v[*sp] = true;
        }
        v
    }
}

impl std::fmt::Debug for PacketNumberSpaceSet {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut first = true;
        f.write_str("(")?;
        for sp in PacketNumberSpace::iter() {
            if self[*sp] {
                if !first {
                    f.write_str("+")?;
                    first = false;
                }
                std::fmt::Display::fmt(sp, f)?;
            }
        }
        f.write_str(")")
    }
}

impl std::fmt::Display for PacketNumberSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Self::Initial => "in",
            Self::Handshake => "hs",
            Self::ApplicationData => "ap",
        })
    }
}

/// `InsertionResult` tracks whether something was inserted for `PacketRange::add()`.
pub enum InsertionResult {
    Largest,
    Smallest,
    NotInserted,
}

#[derive(Clone, Debug, Default)]
pub struct PacketRange {
    largest: PacketNumber,
    smallest: PacketNumber,
    ack_needed: bool,
}

impl PacketRange {
    /// Make a single packet range.
    pub const fn new(pn: PacketNumber) -> Self {
        Self {
            largest: pn,
            smallest: pn,
            ack_needed: true,
        }
    }

    /// Get the number of acknowledged packets in the range.
    pub const fn len(&self) -> u64 {
        self.largest - self.smallest + 1
    }

    /// Returns whether this needs to be sent.
    pub const fn ack_needed(&self) -> bool {
        self.ack_needed
    }

    /// Return whether the given number is in the range.
    pub const fn contains(&self, pn: PacketNumber) -> bool {
        (pn >= self.smallest) && (pn <= self.largest)
    }

    /// Maybe add a packet number to the range.  Returns true if it was added
    /// at the small end (which indicates that this might need merging with a
    /// preceding range).
    pub fn add(&mut self, pn: PacketNumber) -> InsertionResult {
        assert!(!self.contains(pn));
        // Only insert if this is adjacent the current range.
        if (self.largest + 1) == pn {
            qtrace!([self], "Adding largest {}", pn);
            self.largest += 1;
            self.ack_needed = true;
            InsertionResult::Largest
        } else if self.smallest == (pn + 1) {
            qtrace!([self], "Adding smallest {}", pn);
            self.smallest -= 1;
            self.ack_needed = true;
            InsertionResult::Smallest
        } else {
            InsertionResult::NotInserted
        }
    }

    /// Maybe merge a higher-numbered range into this.
    fn merge_larger(&mut self, other: &Self) {
        qinfo!([self], "Merging {}", other);
        // This only works if they are immediately adjacent.
        assert_eq!(self.largest + 1, other.smallest);

        self.largest = other.largest;
        self.ack_needed = self.ack_needed || other.ack_needed;
    }

    /// When a packet containing the range `other` is acknowledged,
    /// clear the `ack_needed` attribute on this.
    /// Requires that other is equal to this, or a larger range.
    pub fn acknowledged(&mut self, other: &Self) {
        if (other.smallest <= self.smallest) && (other.largest >= self.largest) {
            self.ack_needed = false;
        }
    }
}

impl ::std::fmt::Display for PacketRange {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "{}->{}", self.largest, self.smallest)
    }
}

/// The ACK delay we use.
pub const DEFAULT_ACK_DELAY: Duration = Duration::from_millis(20); // 20ms
/// The default number of in-order packets we will receive after
/// largest acknowledged without sending an immediate acknowledgment.
pub const DEFAULT_ACK_PACKET_TOLERANCE: PacketNumber = 1;
const MAX_TRACKED_RANGES: usize = 32;
const MAX_ACKS_PER_FRAME: usize = 32;

/// A structure that tracks what was included in an ACK.
#[derive(Debug, Clone)]
pub struct AckToken {
    space: PacketNumberSpace,
    ranges: Vec<PacketRange>,
}

impl AckToken {
    /// Get the space for this token.
    pub const fn space(&self) -> PacketNumberSpace {
        self.space
    }
}

/// A structure that tracks what packets have been received,
/// and what needs acknowledgement for a packet number space.
#[derive(Debug)]
pub struct RecvdPackets {
    space: PacketNumberSpace,
    ranges: VecDeque<PacketRange>,
    /// The packet number of the lowest number packet that we are tracking.
    min_tracked: PacketNumber,
    /// The time we got the largest acknowledged.
    largest_pn_time: Option<Instant>,
    /// The time that we should be sending an ACK.
    ack_time: Option<Instant>,
    /// The time we last sent an ACK.
    last_ack_time: Option<Instant>,
    /// The current ACK frequency sequence number.
    ack_frequency_seqno: u64,
    /// The time to delay after receiving the first packet that is
    /// not immediately acknowledged.
    ack_delay: Duration,
    /// The number of ack-eliciting packets that have been received, but
    /// not acknowledged.
    unacknowledged_count: PacketNumber,
    /// The number of contiguous packets that can be received without
    /// acknowledging immediately.
    unacknowledged_tolerance: PacketNumber,
    /// Whether we are ignoring packets that arrive out of order
    /// for the purposes of generating immediate acknowledgment.
    ignore_order: bool,
    // The counts of different ECN marks that have been received.
    ecn_count: EcnCount,
}

impl RecvdPackets {
    /// Make a new `RecvdPackets` for the indicated packet number space.
    pub fn new(space: PacketNumberSpace) -> Self {
        Self {
            space,
            ranges: VecDeque::new(),
            min_tracked: 0,
            largest_pn_time: None,
            ack_time: None,
            last_ack_time: None,
            ack_frequency_seqno: 0,
            ack_delay: DEFAULT_ACK_DELAY,
            unacknowledged_count: 0,
            unacknowledged_tolerance: if space == PacketNumberSpace::ApplicationData {
                DEFAULT_ACK_PACKET_TOLERANCE
            } else {
                // ACK more aggressively
                0
            },
            ignore_order: false,
            ecn_count: EcnCount::default(),
        }
    }

    /// Get the ECN counts.
    pub fn ecn_marks(&mut self) -> &mut EcnCount {
        &mut self.ecn_count
    }

    /// Get the time at which the next ACK should be sent.
    pub const fn ack_time(&self) -> Option<Instant> {
        self.ack_time
    }

    /// Update acknowledgment delay parameters.
    pub fn ack_freq(
        &mut self,
        seqno: u64,
        tolerance: PacketNumber,
        delay: Duration,
        ignore_order: bool,
    ) {
        // Yes, this means that we will overwrite values if a sequence number is
        // reused, but that is better than using an `Option<PacketNumber>`
        // when it will always be `Some`.
        if seqno >= self.ack_frequency_seqno {
            self.ack_frequency_seqno = seqno;
            self.unacknowledged_tolerance = tolerance;
            self.ack_delay = delay;
            self.ignore_order = ignore_order;
        }
    }

    /// Returns true if an ACK frame should be sent now.
    fn ack_now(&self, now: Instant, rtt: Duration) -> bool {
        // If ack_time is Some, then we have something to acknowledge.
        // In that case, either ack because `now >= ack_time`, or
        // because it is more than an RTT since the last time we sent an ack.
        self.ack_time.is_some_and(|next| {
            next <= now || self.last_ack_time.is_some_and(|last| last + rtt <= now)
        })
    }

    // A simple addition of a packet number to the tracked set.
    // This doesn't do a binary search on the assumption that
    // new packets will generally be added to the start of the list.
    fn add(&mut self, pn: PacketNumber) {
        for i in 0..self.ranges.len() {
            match self.ranges[i].add(pn) {
                InsertionResult::Largest => return,
                InsertionResult::Smallest => {
                    // If this was the smallest, it might have filled a gap.
                    let nxt = i + 1;
                    if (nxt < self.ranges.len()) && (pn - 1 == self.ranges[nxt].largest) {
                        let larger = self.ranges.remove(i).unwrap();
                        self.ranges[i].merge_larger(&larger);
                    }
                    return;
                }
                InsertionResult::NotInserted => {
                    if self.ranges[i].largest < pn {
                        self.ranges.insert(i, PacketRange::new(pn));
                        return;
                    }
                }
            }
        }
        self.ranges.push_back(PacketRange::new(pn));
    }

    fn trim_ranges(&mut self) {
        // Limit the number of ranges that are tracked to MAX_TRACKED_RANGES.
        if self.ranges.len() > MAX_TRACKED_RANGES {
            let oldest = self.ranges.pop_back().unwrap();
            if oldest.ack_needed {
                qwarn!([self], "Dropping unacknowledged ACK range: {}", oldest);
            // TODO(mt) Record some statistics about this so we can tune MAX_TRACKED_RANGES.
            } else {
                qdebug!([self], "Drop ACK range: {}", oldest);
            }
            self.min_tracked = oldest.largest + 1;
        }
    }

    /// Add the packet to the tracked set.
    /// Return true if the packet was the largest received so far.
    pub fn set_received(&mut self, now: Instant, pn: PacketNumber, ack_eliciting: bool) -> bool {
        let next_in_order_pn = self.ranges.front().map_or(0, |r| r.largest + 1);
        qtrace!([self], "received {}, next: {}", pn, next_in_order_pn);

        self.add(pn);
        self.trim_ranges();

        // The new addition was the largest, so update the time we use for calculating ACK delay.
        let largest = if pn >= next_in_order_pn {
            self.largest_pn_time = Some(now);
            true
        } else {
            false
        };

        if ack_eliciting {
            self.unacknowledged_count += 1;

            let immediate_ack = self.space != PacketNumberSpace::ApplicationData
                || (pn != next_in_order_pn && !self.ignore_order)
                || self.unacknowledged_count > self.unacknowledged_tolerance;

            let ack_time = if immediate_ack {
                now
            } else {
                // Note that `ack_delay` can change and that won't take effect if
                // we are waiting on the previous delay timer.
                // If ACK delay increases, we might send an ACK a bit early;
                // if ACK delay decreases, we might send an ACK a bit later.
                // We could use min() here, but change is rare and the size
                // of the change is very small.
                self.ack_time.unwrap_or_else(|| now + self.ack_delay)
            };
            qdebug!([self], "Set ACK timer to {:?}", ack_time);
            self.ack_time = Some(ack_time);
        }
        largest
    }

    /// If we just received a PING frame, we should immediately acknowledge.
    pub fn immediate_ack(&mut self, now: Instant) {
        self.ack_time = Some(now);
        qdebug!([self], "immediate_ack at {:?}", now);
    }

    /// Check if the packet is a duplicate.
    pub fn is_duplicate(&self, pn: PacketNumber) -> bool {
        if pn < self.min_tracked {
            return true;
        }
        self.ranges
            .iter()
            .take_while(|r| pn <= r.largest)
            .any(|r| r.contains(pn))
    }

    /// Mark the given range as having been acknowledged.
    pub fn acknowledged(&mut self, acked: &[PacketRange]) {
        let mut range_iter = self.ranges.iter_mut();
        let mut cur = range_iter.next().expect("should have at least one range");
        for ack in acked {
            while cur.smallest > ack.largest {
                cur = match range_iter.next() {
                    Some(c) => c,
                    None => return,
                };
            }
            cur.acknowledged(ack);
        }
    }

    /// Length of the worst possible ACK frame, assuming only one range and ECN counts.
    /// Note that this assumes one byte for the type and count of extra ranges.
    pub const USEFUL_ACK_LEN: usize = 1 + 8 + 8 + 1 + 8 + 3 * 8;

    /// Generate an ACK frame for this packet number space.
    ///
    /// Unlike other frame generators this doesn't modify the underlying instance
    /// to track what has been sent. This only clears the delayed ACK timer.
    ///
    /// When sending ACKs, we want to always send the most recent ranges,
    /// even if they have been sent in other packets.
    ///
    /// We don't send ranges that have been acknowledged, but they still need
    /// to be tracked so that duplicates can be detected.
    fn write_frame(
        &mut self,
        now: Instant,
        rtt: Duration,
        builder: &mut PacketBuilder,
        tokens: &mut Vec<RecoveryToken>,
        stats: &mut FrameStats,
    ) {
        // Check that we aren't delaying ACKs.
        if !self.ack_now(now, rtt) {
            return;
        }

        // Drop extra ACK ranges to fit the available space.  Do this based on
        // a worst-case estimate of frame size for simplicity.
        //
        // When congestion limited, ACK-only packets are 255 bytes at most
        // (`recovery::ACK_ONLY_SIZE_LIMIT - 1`).  This results in limiting the
        // ranges to 13 here.
        let max_ranges = if let Some(avail) = builder.remaining().checked_sub(Self::USEFUL_ACK_LEN)
        {
            // Apply a hard maximum to keep plenty of space for other stuff.
            min(1 + (avail / 16), MAX_ACKS_PER_FRAME)
        } else {
            return;
        };

        let ranges = self
            .ranges
            .iter()
            .filter(|r| r.ack_needed())
            .take(max_ranges)
            .cloned()
            .collect::<Vec<_>>();
        if ranges.is_empty() {
            return;
        }

        builder.encode_varint(if self.ecn_count.is_some() {
            FRAME_TYPE_ACK_ECN
        } else {
            FRAME_TYPE_ACK
        });
        let mut iter = ranges.iter();
        let Some(first) = iter.next() else { return };
        builder.encode_varint(first.largest);
        stats.largest_acknowledged = first.largest;
        stats.ack += 1;

        let elapsed = now.duration_since(self.largest_pn_time.unwrap());
        // We use the default exponent, so delay is in multiples of 8 microseconds.
        let ack_delay = u64::try_from(elapsed.as_micros() / 8).unwrap_or(u64::MAX);
        let ack_delay = min((1 << 62) - 1, ack_delay);
        builder.encode_varint(ack_delay);
        builder.encode_varint(u64::try_from(ranges.len() - 1).unwrap()); // extra ranges
        builder.encode_varint(first.len() - 1); // first range

        let mut last = first.smallest;
        for r in iter {
            // the difference must be at least 2 because 0-length gaps,
            // (difference 1) are illegal.
            builder.encode_varint(last - r.largest - 2); // Gap
            builder.encode_varint(r.len() - 1); // Range
            last = r.smallest;
        }

        if self.ecn_count.is_some() {
            builder.encode_varint(self.ecn_count[IpTosEcn::Ect0]);
            builder.encode_varint(self.ecn_count[IpTosEcn::Ect1]);
            builder.encode_varint(self.ecn_count[IpTosEcn::Ce]);
        }

        // We've sent an ACK, reset the timer.
        self.ack_time = None;
        self.last_ack_time = Some(now);
        self.unacknowledged_count = 0;

        tokens.push(RecoveryToken::Ack(AckToken {
            space: self.space,
            ranges,
        }));
    }
}

impl ::std::fmt::Display for RecvdPackets {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "Recvd-{}", self.space)
    }
}

pub struct AckTracker {
    spaces: EnumMap<PacketNumberSpace, Option<RecvdPackets>>,
}

impl AckTracker {
    pub fn drop_space(&mut self, space: PacketNumberSpace) {
        assert_ne!(
            space,
            PacketNumberSpace::ApplicationData,
            "discarding application space"
        );
        if space == PacketNumberSpace::Handshake {
            assert!(self.spaces[PacketNumberSpace::Initial].is_none());
        }
        self.spaces[space].take();
    }

    pub fn get_mut(&mut self, space: PacketNumberSpace) -> Option<&mut RecvdPackets> {
        self.spaces[space].as_mut()
    }

    pub fn ack_freq(
        &mut self,
        seqno: u64,
        tolerance: PacketNumber,
        delay: Duration,
        ignore_order: bool,
    ) {
        // Only ApplicationData ever delays ACK.
        if let Some(space) = self.get_mut(PacketNumberSpace::ApplicationData) {
            space.ack_freq(seqno, tolerance, delay, ignore_order);
        }
    }

    /// Force an ACK to be generated immediately.
    pub fn immediate_ack(&mut self, space: PacketNumberSpace, now: Instant) {
        if let Some(space) = self.get_mut(space) {
            space.immediate_ack(now);
        }
    }

    /// Determine the earliest time that an ACK might be needed.
    pub fn ack_time(&self, now: Instant) -> Option<Instant> {
        #[cfg(debug_assertions)]
        for (space, recvd) in &self.spaces {
            if let Some(recvd) = recvd {
                qtrace!("ack_time for {} = {:?}", space, recvd.ack_time());
            }
        }

        if self.spaces[PacketNumberSpace::Initial].is_none()
            && self.spaces[PacketNumberSpace::Handshake].is_none()
        {
            if let Some(recvd) = &self.spaces[PacketNumberSpace::ApplicationData] {
                return recvd.ack_time();
            }
        }

        // Ignore any time that is in the past relative to `now`.
        // That is something of a hack, but there are cases where we can't send ACK
        // frames for all spaces, which can mean that one space is stuck in the past.
        // That isn't a problem because we guarantee that earlier spaces will always
        // be able to send ACK frames.
        self.spaces
            .values()
            .flatten()
            .filter_map(|recvd| recvd.ack_time().filter(|t| *t > now))
            .min()
    }

    pub fn acked(&mut self, token: &AckToken) {
        if let Some(space) = self.get_mut(token.space) {
            space.acknowledged(&token.ranges);
        }
    }

    pub(crate) fn write_frame(
        &mut self,
        pn_space: PacketNumberSpace,
        now: Instant,
        rtt: Duration,
        builder: &mut PacketBuilder,
        tokens: &mut Vec<RecoveryToken>,
        stats: &mut FrameStats,
    ) {
        if let Some(space) = self.get_mut(pn_space) {
            space.write_frame(now, rtt, builder, tokens, stats);
        }
    }
}

impl Default for AckTracker {
    fn default() -> Self {
        Self {
            spaces: enum_map! {
                PacketNumberSpace::Initial => Some(RecvdPackets::new(PacketNumberSpace::Initial)),
                PacketNumberSpace::Handshake => Some(RecvdPackets::new(PacketNumberSpace::Handshake)),
                PacketNumberSpace::ApplicationData => Some(RecvdPackets::new(PacketNumberSpace::ApplicationData)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use neqo_common::Encoder;
    use test_fixture::now;

    use super::{
        AckTracker, Duration, Instant, PacketNumberSpace, PacketNumberSpaceSet, RecoveryToken,
        RecvdPackets, MAX_TRACKED_RANGES,
    };
    use crate::{
        frame::Frame,
        packet::{PacketBuilder, PacketNumber, PacketType},
        stats::FrameStats,
    };

    const RTT: Duration = Duration::from_millis(100);

    fn test_ack_range(pns: &[PacketNumber], nranges: usize) {
        let mut rp = RecvdPackets::new(PacketNumberSpace::Initial); // Any space will do.
        let mut packets = HashSet::new();

        for pn in pns {
            rp.set_received(now(), *pn, true);
            packets.insert(*pn);
        }

        assert_eq!(rp.ranges.len(), nranges);

        // Check that all these packets will be detected as duplicates.
        for pn in pns {
            assert!(rp.is_duplicate(*pn));
        }

        // Check that the ranges decrease monotonically and don't overlap.
        let mut iter = rp.ranges.iter();
        let mut last = iter.next().expect("should have at least one");
        for n in iter {
            assert!(n.largest + 1 < last.smallest);
            last = n;
        }

        // Check that the ranges include the right values.
        let mut in_ranges = HashSet::new();
        for range in &rp.ranges {
            for included in range.smallest..=range.largest {
                in_ranges.insert(included);
            }
        }
        assert_eq!(packets, in_ranges);
    }

    #[test]
    fn pn0() {
        test_ack_range(&[0], 1);
    }

    #[test]
    fn pn1() {
        test_ack_range(&[1], 1);
    }

    #[test]
    fn two_ranges() {
        test_ack_range(&[0, 1, 2, 5, 6, 7], 2);
    }

    #[test]
    fn fill_in_range() {
        test_ack_range(&[0, 1, 2, 5, 6, 7, 3, 4], 1);
    }

    #[test]
    fn too_many_ranges() {
        let mut rp = RecvdPackets::new(PacketNumberSpace::Initial); // Any space will do.

        // This will add one too many disjoint ranges.
        for i in 0..=MAX_TRACKED_RANGES {
            rp.set_received(now(), (i * 2) as u64, true);
        }

        assert_eq!(rp.ranges.len(), MAX_TRACKED_RANGES);
        assert_eq!(rp.ranges.back().unwrap().largest, 2);

        // Even though the range was dropped, we still consider it a duplicate.
        assert!(rp.is_duplicate(0));
        assert!(!rp.is_duplicate(1));
        assert!(rp.is_duplicate(2));
    }

    #[test]
    fn ack_delay() {
        const COUNT: PacketNumber = 9;
        const DELAY: Duration = Duration::from_millis(7);
        // Only application data packets are delayed.
        let mut rp = RecvdPackets::new(PacketNumberSpace::ApplicationData);
        assert!(rp.ack_time().is_none());
        assert!(!rp.ack_now(now(), RTT));

        rp.ack_freq(0, COUNT, DELAY, false);

        // Some packets won't cause an ACK to be needed.
        for i in 0..COUNT {
            rp.set_received(now(), i, true);
            assert_eq!(Some(now() + DELAY), rp.ack_time());
            assert!(!rp.ack_now(now(), RTT));
            assert!(rp.ack_now(now() + DELAY, RTT));
        }

        // Exceeding COUNT will move the ACK time to now.
        rp.set_received(now(), COUNT, true);
        assert_eq!(Some(now()), rp.ack_time());
        assert!(rp.ack_now(now(), RTT));
    }

    #[test]
    fn no_ack_delay() {
        for space in &[PacketNumberSpace::Initial, PacketNumberSpace::Handshake] {
            let mut rp = RecvdPackets::new(*space);
            assert!(rp.ack_time().is_none());
            assert!(!rp.ack_now(now(), RTT));

            // Any packet in these spaces is acknowledged straight away.
            rp.set_received(now(), 0, true);
            assert_eq!(Some(now()), rp.ack_time());
            assert!(rp.ack_now(now(), RTT));
        }
    }

    #[test]
    fn ooo_no_ack_delay_new() {
        let mut rp = RecvdPackets::new(PacketNumberSpace::ApplicationData);
        assert!(rp.ack_time().is_none());
        assert!(!rp.ack_now(now(), RTT));

        // Anything other than packet 0 is acknowledged immediately.
        rp.set_received(now(), 1, true);
        assert_eq!(Some(now()), rp.ack_time());
        assert!(rp.ack_now(now(), RTT));
    }

    fn write_frame_at(rp: &mut RecvdPackets, now: Instant) {
        let mut builder = PacketBuilder::short(Encoder::new(), false, None::<&[u8]>);
        let mut stats = FrameStats::default();
        let mut tokens = Vec::new();
        rp.write_frame(now, RTT, &mut builder, &mut tokens, &mut stats);
        assert!(!tokens.is_empty());
        assert_eq!(stats.ack, 1);
    }

    fn write_frame(rp: &mut RecvdPackets) {
        write_frame_at(rp, now());
    }

    #[test]
    fn ooo_no_ack_delay_fill() {
        let mut rp = RecvdPackets::new(PacketNumberSpace::ApplicationData);
        rp.set_received(now(), 1, true);
        write_frame(&mut rp);

        // Filling in behind the largest acknowledged causes immediate ACK.
        rp.set_received(now(), 0, true);
        write_frame(&mut rp);

        // Receiving the next packet won't elicit an ACK.
        rp.set_received(now(), 2, true);
        assert!(!rp.ack_now(now(), RTT));
    }

    #[test]
    fn immediate_ack_after_rtt() {
        let mut rp = RecvdPackets::new(PacketNumberSpace::ApplicationData);
        rp.set_received(now(), 1, true);
        write_frame(&mut rp);

        // Filling in behind the largest acknowledged causes immediate ACK.
        rp.set_received(now(), 0, true);
        write_frame(&mut rp);

        // A new packet ordinarily doesn't result in an ACK, but this time it does.
        rp.set_received(now() + RTT, 2, true);
        write_frame_at(&mut rp, now() + RTT);
    }

    #[test]
    fn ooo_no_ack_delay_threshold_new() {
        let mut rp = RecvdPackets::new(PacketNumberSpace::ApplicationData);

        // Set tolerance to 2 and then it takes three packets.
        rp.ack_freq(0, 2, Duration::from_millis(10), true);

        rp.set_received(now(), 1, true);
        assert_ne!(Some(now()), rp.ack_time());
        rp.set_received(now(), 2, true);
        assert_ne!(Some(now()), rp.ack_time());
        rp.set_received(now(), 3, true);
        assert_eq!(Some(now()), rp.ack_time());
    }

    #[test]
    fn ooo_no_ack_delay_threshold_gap() {
        let mut rp = RecvdPackets::new(PacketNumberSpace::ApplicationData);
        rp.set_received(now(), 1, true);
        write_frame(&mut rp);

        // Set tolerance to 2 and then it takes three packets.
        rp.ack_freq(0, 2, Duration::from_millis(10), true);

        rp.set_received(now(), 3, true);
        assert_ne!(Some(now()), rp.ack_time());
        rp.set_received(now(), 4, true);
        assert_ne!(Some(now()), rp.ack_time());
        rp.set_received(now(), 5, true);
        assert_eq!(Some(now()), rp.ack_time());
    }

    /// Test that an in-order packet that is not ack-eliciting doesn't
    /// increase the number of packets needed to cause an ACK.
    #[test]
    fn non_ack_eliciting_skip() {
        let mut rp = RecvdPackets::new(PacketNumberSpace::ApplicationData);
        rp.ack_freq(0, 1, Duration::from_millis(10), true);

        // This should be ignored.
        rp.set_received(now(), 0, false);
        assert_ne!(Some(now()), rp.ack_time());
        // Skip 1 (it has no effect).
        rp.set_received(now(), 2, true);
        assert_ne!(Some(now()), rp.ack_time());
        rp.set_received(now(), 3, true);
        assert_eq!(Some(now()), rp.ack_time());
    }

    /// If a packet that is not ack-eliciting is reordered, that's fine too.
    #[test]
    fn non_ack_eliciting_reorder() {
        let mut rp = RecvdPackets::new(PacketNumberSpace::ApplicationData);
        rp.ack_freq(0, 1, Duration::from_millis(10), false);

        // These are out of order, but they are not ack-eliciting.
        rp.set_received(now(), 1, false);
        assert_ne!(Some(now()), rp.ack_time());
        rp.set_received(now(), 0, false);
        assert_ne!(Some(now()), rp.ack_time());

        // These are in order.
        rp.set_received(now(), 2, true);
        assert_ne!(Some(now()), rp.ack_time());
        rp.set_received(now(), 3, true);
        assert_eq!(Some(now()), rp.ack_time());
    }

    #[test]
    fn aggregate_ack_time() {
        const DELAY: Duration = Duration::from_millis(17);
        let mut tracker = AckTracker::default();
        tracker.ack_freq(0, 1, DELAY, false);
        // This packet won't trigger an ACK.
        tracker
            .get_mut(PacketNumberSpace::Handshake)
            .unwrap()
            .set_received(now(), 0, false);
        assert_eq!(None, tracker.ack_time(now()));

        // This should be delayed.
        tracker
            .get_mut(PacketNumberSpace::ApplicationData)
            .unwrap()
            .set_received(now(), 0, true);
        assert_eq!(Some(now() + DELAY), tracker.ack_time(now()));

        // This should move the time forward.
        let later = now() + (DELAY / 2);
        tracker
            .get_mut(PacketNumberSpace::Initial)
            .unwrap()
            .set_received(later, 0, true);
        assert_eq!(Some(later), tracker.ack_time(now()));
    }

    #[test]
    #[should_panic(expected = "discarding application space")]
    fn drop_app() {
        let mut tracker = AckTracker::default();
        tracker.drop_space(PacketNumberSpace::ApplicationData);
    }

    #[test]
    fn drop_spaces() {
        let mut tracker = AckTracker::default();
        let mut builder = PacketBuilder::short(Encoder::new(), false, None::<&[u8]>);
        tracker
            .get_mut(PacketNumberSpace::Initial)
            .unwrap()
            .set_received(now(), 0, true);
        // The reference time for `ack_time` has to be in the past or we filter out the timer.
        assert!(tracker
            .ack_time(now().checked_sub(Duration::from_millis(1)).unwrap())
            .is_some());

        let mut tokens = Vec::new();
        let mut stats = FrameStats::default();
        tracker.write_frame(
            PacketNumberSpace::Initial,
            now(),
            RTT,
            &mut builder,
            &mut tokens,
            &mut stats,
        );
        assert_eq!(stats.ack, 1);

        // Mark another packet as received so we have cause to send another ACK in that space.
        tracker
            .get_mut(PacketNumberSpace::Initial)
            .unwrap()
            .set_received(now(), 1, true);
        assert!(tracker
            .ack_time(now().checked_sub(Duration::from_millis(1)).unwrap())
            .is_some());

        // Now drop that space.
        tracker.drop_space(PacketNumberSpace::Initial);

        assert!(tracker.get_mut(PacketNumberSpace::Initial).is_none());
        assert!(tracker
            .ack_time(now().checked_sub(Duration::from_millis(1)).unwrap())
            .is_none());
        tracker.write_frame(
            PacketNumberSpace::Initial,
            now(),
            RTT,
            &mut builder,
            &mut tokens,
            &mut stats,
        );
        assert_eq!(stats.ack, 1);
        if let RecoveryToken::Ack(tok) = &tokens[0] {
            tracker.acked(tok); // Should be a noop.
        } else {
            panic!("not an ACK token");
        }
    }

    #[test]
    fn no_room_for_ack() {
        let mut tracker = AckTracker::default();
        tracker
            .get_mut(PacketNumberSpace::Initial)
            .unwrap()
            .set_received(now(), 0, true);
        assert!(tracker
            .ack_time(now().checked_sub(Duration::from_millis(1)).unwrap())
            .is_some());

        let mut builder = PacketBuilder::short(Encoder::new(), false, None::<&[u8]>);
        builder.set_limit(10);

        let mut stats = FrameStats::default();
        tracker.write_frame(
            PacketNumberSpace::Initial,
            now(),
            RTT,
            &mut builder,
            &mut Vec::new(),
            &mut stats,
        );
        assert_eq!(stats.ack, 0);
        assert_eq!(builder.len(), 1); // Only the short packet header has been added.
    }

    #[test]
    fn no_room_for_extra_range() {
        let mut tracker = AckTracker::default();
        tracker
            .get_mut(PacketNumberSpace::Initial)
            .unwrap()
            .set_received(now(), 0, true);
        tracker
            .get_mut(PacketNumberSpace::Initial)
            .unwrap()
            .set_received(now(), 2, true);
        assert!(tracker
            .ack_time(now().checked_sub(Duration::from_millis(1)).unwrap())
            .is_some());

        let mut builder = PacketBuilder::short(Encoder::new(), false, None::<&[u8]>);
        // The code pessimistically assumes that each range needs 16 bytes to express.
        // So this won't be enough for a second range.
        builder.set_limit(RecvdPackets::USEFUL_ACK_LEN + 8);

        let mut stats = FrameStats::default();
        tracker.write_frame(
            PacketNumberSpace::Initial,
            now(),
            RTT,
            &mut builder,
            &mut Vec::new(),
            &mut stats,
        );
        assert_eq!(stats.ack, 1);

        let mut dec = builder.as_decoder();
        dec.skip(1); // Skip the short header.
        let frame = Frame::decode(&mut dec).unwrap();
        if let Frame::Ack { ack_ranges, .. } = frame {
            assert_eq!(ack_ranges.len(), 0);
        } else {
            panic!("not an ACK!");
        }
    }

    #[test]
    fn ack_time_elapsed() {
        let mut tracker = AckTracker::default();

        // While we have multiple PN spaces, we ignore ACK timers from the past.
        // Send out of order to cause the delayed ack timer to be set to `now()`.
        tracker
            .get_mut(PacketNumberSpace::ApplicationData)
            .unwrap()
            .set_received(now(), 3, true);
        assert!(tracker.ack_time(now() + Duration::from_millis(1)).is_none());

        // When we are reduced to one space, that filter is off.
        tracker.drop_space(PacketNumberSpace::Initial);
        tracker.drop_space(PacketNumberSpace::Handshake);
        assert_eq!(
            tracker.ack_time(now() + Duration::from_millis(1)),
            Some(now())
        );
    }

    #[test]
    fn pnspaceset_default() {
        let set = PacketNumberSpaceSet::default();
        assert!(!set[PacketNumberSpace::Initial]);
        assert!(!set[PacketNumberSpace::Handshake]);
        assert!(!set[PacketNumberSpace::ApplicationData]);
    }

    #[test]
    fn pnspaceset_from() {
        let set = PacketNumberSpaceSet::from(&[PacketNumberSpace::Initial]);
        assert!(set[PacketNumberSpace::Initial]);
        assert!(!set[PacketNumberSpace::Handshake]);
        assert!(!set[PacketNumberSpace::ApplicationData]);

        let set =
            PacketNumberSpaceSet::from(&[PacketNumberSpace::Handshake, PacketNumberSpace::Initial]);
        assert!(set[PacketNumberSpace::Initial]);
        assert!(set[PacketNumberSpace::Handshake]);
        assert!(!set[PacketNumberSpace::ApplicationData]);

        let set = PacketNumberSpaceSet::from(&[
            PacketNumberSpace::ApplicationData,
            PacketNumberSpace::ApplicationData,
        ]);
        assert!(!set[PacketNumberSpace::Initial]);
        assert!(!set[PacketNumberSpace::Handshake]);
        assert!(set[PacketNumberSpace::ApplicationData]);
    }

    #[test]
    fn pnspaceset_copy() {
        let set = PacketNumberSpaceSet::from(&[
            PacketNumberSpace::Handshake,
            PacketNumberSpace::ApplicationData,
        ]);
        let copy = set;
        assert!(!copy[PacketNumberSpace::Initial]);
        assert!(copy[PacketNumberSpace::Handshake]);
        assert!(copy[PacketNumberSpace::ApplicationData]);
    }

    #[test]
    fn from_packet_type() {
        assert_eq!(
            PacketNumberSpace::from(PacketType::Initial),
            PacketNumberSpace::Initial
        );
        assert_eq!(
            PacketNumberSpace::from(PacketType::Handshake),
            PacketNumberSpace::Handshake
        );
        assert_eq!(
            PacketNumberSpace::from(PacketType::ZeroRtt),
            PacketNumberSpace::ApplicationData
        );
        assert_eq!(
            PacketNumberSpace::from(PacketType::Short),
            PacketNumberSpace::ApplicationData
        );
        assert!(std::panic::catch_unwind(|| {
            PacketNumberSpace::from(PacketType::VersionNegotiation)
        })
        .is_err());
    }
}
