pub use self::ffi::*;

#[cxx::bridge(namespace = "webrtc")]
pub mod ffi {
    #[derive(Debug, Eq, Hash, PartialEq)]
    #[repr(i32)]
    pub enum BandwidthUsage {
        kBwNormal,
        kBwUnderusing,
        kBwOverusing,
        kLast,
    }

    unsafe extern "C++" {
        include!("/media/alexlapa/drive2/CLionProjects3/str0m-fork/include/bridge.h");

        pub type BandwidthUsage;

        //------------------ TrendlineEstimator
        pub type TrendlineEstimator;

        pub fn new_trendline_estimator() -> UniquePtr<TrendlineEstimator>;

        pub fn Update(
            self: Pin<&mut TrendlineEstimator>,
            recv_delta_ms: f64,
            send_delta_ms: f64,
            arrival_time_ms: i64,
        );

        pub fn State(self: &TrendlineEstimator) -> BandwidthUsage;

        pub fn num_of_deltas(self: &TrendlineEstimator) -> i32;

        pub fn accumulated_delay(self: &TrendlineEstimator) -> f64;

        pub fn smoothed_delay(self: &TrendlineEstimator) -> f64;

        //------------------ InterArrivalDelta

        pub type InterArrivalDelta;

        pub fn new_inter_arrival_delta() -> UniquePtr<InterArrivalDelta>;

        // This function returns true if a delta was computed, or false if the current
        // group is still incomplete or if only one group has been completed.
        //
        // `send_time` is the send time.
        // `arrival_time` is the time at which the packet arrived.
        // `packet_size` is the size of the packet.
        // `timestamp_delta` (output) is the computed send time delta.
        // `arrival_time_delta` (output) is the computed arrival-time delta.
        // `packet_size_delta` (output) is the computed size delta.
        //
        // bool ComputeDeltas(
        //      Timestamp send_time,            // packet_feedback.sent_packet.send_time,
        //      Timestamp arrival_time,         // packet_feedback.receive_time,
        //      Timestamp system_time,          // at_time
        //      size_t packet_size,             // packet_size.bytes(),
        //      TimeDelta* send_time_delta,     // &send_delta,
        //      TimeDelta* arrival_time_delta,  // &recv_delta,
        //      int* packet_size_delta          // &size_delta
        // );
        pub fn ComputeDeltas(
            iad: Pin<&mut InterArrivalDelta>,
            send_time_us: u64,
            arrival_time_us: u64,
            system_time_us: u64,
            packet_size_bytes: u64,
            send_time_delta_us: &mut u64,
            arrival_time_delta_us: &mut u64,
            packet_size_delta: &mut u64,
        ) -> bool;

    }
}

unsafe impl Send for TrendlineEstimator {}
unsafe impl Send for InterArrivalDelta {}
