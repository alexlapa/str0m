#ifndef BRIDGE_H_
#define BRIDGE_H_

#include <cassert>
#include <chrono>
#include <iostream>
#include <memory>

#include "time_delta.h"
#include "bandwidth_usage.h"
#include "trendline_estimator.h"
#include "inter_arrival_delta.h"

namespace webrtc {

    std::unique_ptr<webrtc::TrendlineEstimator> new_trendline_estimator();

    std::unique_ptr<webrtc::InterArrivalDelta> new_inter_arrival_delta();

    bool ComputeDeltas(
        webrtc::InterArrivalDelta& self,
        uint64_t send_time_us,
        uint64_t arrival_time_us,
        uint64_t system_time_us,
        uint64_t packet_size,
        uint64_t& send_time_delta_us,
        uint64_t& arrival_time_delta_us,
        uint64_t& packet_size_delta);

}  // namespace bridge

#endif  // BRIDGE_H_
