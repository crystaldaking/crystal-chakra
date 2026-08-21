#pragma once

#include <string>

namespace chakra::payments {

inline int provider_refund() { return 1; }
inline int service_refund() { return provider_refund(); }
inline int controller_refund() { return service_refund(); }

class Provider {
 public:
  virtual ~Provider() = default;
  virtual std::string execute_refund(const std::string& payment_id) = 0;
};

class PaymentProvider final : public Provider {
 public:
  std::string execute_refund(const std::string& payment_id) override {
    return "provider:" + payment_id;
  }
};

class PaymentService {
 public:
  explicit PaymentService(PaymentProvider& provider) : provider_(provider) {}
  std::string refund(const std::string& payment_id) {
    return provider_.execute_refund(payment_id);
  }

 private:
  PaymentProvider& provider_;
};

class PaymentController {
 public:
  explicit PaymentController(PaymentService& service) : service_(service) {}
  std::string handle_refund(const std::string& payment_id) {
    return service_.refund(payment_id);
  }

 private:
  PaymentService& service_;
};

}  // namespace chakra::payments
