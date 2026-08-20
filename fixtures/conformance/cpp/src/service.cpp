#include "shared.hpp"

// CONFORMANCE_TEXT_NEEDLE: payment pipeline marker

namespace chakra::conformance {

void dispatch_conformance_request() {
  shared_unique_target();
  record_conformance_event();
}

}  // namespace chakra::conformance
