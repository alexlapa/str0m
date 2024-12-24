use std::time::{Duration, Instant};

use super::AckedPacket;
use crate::rtp_::SeqNo;

const BURST_DELTA_THRESHOLD: Duration = Duration::from_millis(5);
const SEND_TIME_GROUP_LENGTH: Duration = Duration::from_millis(5);
const MAX_BURST_DURATION: Duration = Duration::from_millis(100);
const REORDERED_RESET_THRESHOLD: usize = 3;

#[derive(Debug, Default)]
pub struct ArrivalGroupAccumulator {
    previous_group: Option<ArrivalGroup>,
    current_group: Option<ArrivalGroup>,
    num_consecutive_reordered_packets: usize,
}

impl ArrivalGroupAccumulator {
    ///
    /// Accumulate a packet.
    ///
    /// If adding this packet produced a new delay delta it is returned.
    pub(super) fn compute_deltas(&mut self, packet: &AckedPacket) -> Option<InterGroupDelayDelta> {
        let Some(current_group) = &mut self.current_group else {
            // We don't have enough data to update the filter, so we store it until we
            // have two frames of data to process.
            self.current_group = Some(ArrivalGroup {
                first_seq_no: packet.seq_no,
                first_send_time: packet.local_send_time,
                first_arrival: packet.remote_recv_time,
                last_seq_no: packet.seq_no,
                send_time: packet.local_send_time,
                complete_time: packet.remote_recv_time,
                size: 1,
            });

            return None;
        };

        let mut send_time_delta = None;
        let mut arrival_time_delta = None;
        if current_group.first_send_time > packet.local_send_time {
            // Reordered packet.
            return None;
        } else if current_group.new_timestamp_group(packet.remote_recv_time, packet.local_send_time)
        {
            // First packet of a later send burst, the previous packets sample is ready.
            if self.previous_group.is_some() {
                let previous_group = self.previous_group.as_mut().unwrap();
                send_time_delta = Some(current_group.send_time - previous_group.send_time);
                arrival_time_delta =
                    Some(current_group.complete_time - previous_group.complete_time);

                if arrival_time_delta.unwrap() < Duration::ZERO {
                    // The group of packets has been reordered since receiving its local
                    // arrival timestamp.
                    self.num_consecutive_reordered_packets += 1;
                    if self.num_consecutive_reordered_packets >= REORDERED_RESET_THRESHOLD {
                        self.current_group = None;
                        self.previous_group = None;
                        self.num_consecutive_reordered_packets = 0;
                    }

                    return None;
                } else {
                    self.num_consecutive_reordered_packets = 0;
                }
            }

            self.previous_group = Some(*current_group);
            // The new timestamp is now the current frame.
            current_group.first_send_time = packet.local_send_time;
            current_group.send_time = packet.local_send_time;
            current_group.first_arrival = packet.remote_recv_time;
            current_group.size = 0;
        } else {
            current_group.send_time = current_group.send_time.max(packet.local_send_time);
        }

        current_group.size += 1;
        current_group.complete_time = packet.remote_recv_time;

        let send_time_delta = send_time_delta?;
        let arrival_time_delta = arrival_time_delta?;

        return Some(InterGroupDelayDelta {
            send_time_delta,
            arrival_time_delta,
            last_remote_recv_time: current_group.complete_time,
        });
    }
}

#[derive(Debug, Clone, Copy)]
struct ArrivalGroup {
    first_seq_no: SeqNo,
    first_send_time: Instant,
    first_arrival: Instant,
    last_seq_no: SeqNo,
    send_time: Instant,
    complete_time: Instant,
    size: usize,
}

impl ArrivalGroup {
    fn new_timestamp_group(&self, arrival_time: Instant, send_time: Instant) -> bool {
        if self.belongs_to_burst(arrival_time, send_time) {
            return false;
        } else {
            return send_time - self.first_send_time > SEND_TIME_GROUP_LENGTH;
        }
    }

    fn belongs_to_burst(&self, arrival_time: Instant, send_time: Instant) -> bool {
        let arrival_time_delta = arrival_time - self.complete_time;
        let send_time_delta = send_time - self.send_time;

        if send_time_delta == Duration::ZERO {
            return true;
        }

        let propagation_delta = arrival_time_delta.as_secs_f64() - send_time_delta.as_secs_f64();
        if propagation_delta < 0.0
            && arrival_time_delta <= BURST_DELTA_THRESHOLD
            && arrival_time - self.first_arrival < MAX_BURST_DURATION
        {
            return true;
        }

        return false;
    }
}

/// The calculate delay delta between two groups of packets.
#[derive(Debug, Clone, Copy)]
pub(super) struct InterGroupDelayDelta {
    /// The delta between the send times of the two groups i.e. delta between the last packet sent
    /// in each group.
    pub send_time_delta: Duration,
    /// The delay delta between the two groups.
    pub arrival_time_delta: Duration,
    /// The reported receive time for the last packet in the first arrival group.
    pub last_remote_recv_time: Instant,
}
